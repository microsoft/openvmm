// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Process Loop that handles asynchronous requests and notifications to/from
//! the Host via the GET

use self::msg::Msg;
use crate::api::GuestSaveRequest;
use crate::client::ModifyVtl2SettingsRequest;
use crate::error::IgvmAttestError;
use crate::error::TryIntoProtocolBool;
use chipset_resources::battery::HostBatteryUpdate;
use cvm_tracing::CVM_ALLOWED;
use futures::FutureExt;
use futures::TryFutureExt;
use futures_concurrency::future::Race;
use get_protocol::HostRequests;
use get_protocol::MAX_PAYLOAD_SIZE;
use guid::Guid;
use inspect::Inspect;
use inspect::InspectMut;
use inspect_counters::Counter;
use mesh::RecvError;
use mesh::rpc::Rpc;
use mesh::rpc::RpcError;
use mesh::rpc::TryRpcSend;
use parking_lot::Mutex;
use std::cmp::min;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::future::Future;
use std::future::pending;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use underhill_config::Vtl2SettingsErrorInfo;
use underhill_config::Vtl2SettingsErrorInfoVec;
use unicycle::FuturesUnordered;
use user_driver::DmaClient;
use vmbus_async::async_dgram::AsyncRecvExt;
use vmbus_async::async_dgram::AsyncSendExt;
use vmbus_async::pipe::MessagePipe;
use vmbus_ring::RingMem;
use vpci::bus_control::VpciBusEvent;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Error type for GET errors
#[derive(Debug, Error)]
pub(crate) enum FatalError {
    #[error("open get pipe error")]
    OpenPipe(#[source] vmbus_user_channel::Error),
    #[error("get fd io error")]
    FdIo(#[source] std::io::Error),
    #[error("message size of {0} too small to read header")]
    MessageSizeHeader(usize),
    #[error("message size of {len} was not correct to read host response {response:?}")]
    MessageSizeHostResponse { len: usize, response: HostRequests },
    #[error("message size of {len} did not match dpsv2 size {expected}")]
    DevicePlatformSettingsV2Payload { expected: usize, len: usize },
    #[error("message size of {len} did not match vtl2 setting size {expected}")]
    ModifyVtl2SettingsNotification { expected: usize, len: usize },
    #[error("message size of {len} was not correct to read guest notification {notification:?}")]
    MessageSizeGuestNotification {
        len: usize,
        notification: get_protocol::GuestNotifications,
    },
    #[error("response message version ({0:?}) not supported")]
    InvalidResponseVersion(get_protocol::MessageVersions),
    #[error("notification message version ({0:?}) not supported")]
    InvalidGuestNotificationVersion(get_protocol::MessageVersions),
    #[error("response message type ({0:?}) is not HOST_RESPONSE")]
    InvalidResponseType(get_protocol::MessageTypes),
    #[error("response header message ID {0:?} doesn't match request header message ID {1:?}")]
    ResponseHeaderMismatchId(HostRequests, HostRequests),
    #[error("invalid response")]
    InvalidResponse,
    #[error("version negotiation failed")]
    VersionNegotiationFailed,
    #[error("control receive failed")]
    VersionNegotiationTryRecvFailed(#[source] RecvError),
    #[error("failed to serialize VTL2 settings error info")]
    Vtl2SettingsErrorInfoJson(#[source] serde_json::error::Error),
    #[error("received too many guest notifications of kind {0:?} prior to downstream worker init")]
    TooManyGuestNotifications(get_protocol::GuestNotifications),
    #[error("failed to create IgvmAttest request because the gpa allocator is unavailable")]
    GpaAllocatorUnavailable,
    #[error("failed to allocate memory for attestation request")]
    GpaMemoryAllocationError(#[source] anyhow::Error),
    #[error("failed to deserialize the asynchronous `IGVM_ATTEST` response")]
    DeserializeIgvmAttestResponse,
    #[error(
        "malformed `IGVM_ATTEST` response - reported size {response_size} was larger than maximum size {maximum_size}"
    )]
    InvalidIgvmAttestResponseSize {
        response_size: usize,
        maximum_size: usize,
    },
}

type HostRequestQueue = VecDeque<Pin<Box<dyn Future<Output = Result<(), FatalError>> + Send>>>;
type ExclusiveHostRequestResponseReceiverAccess = Arc<Mutex<Option<mesh::Receiver<Vec<u8>>>>>;

/// Validates the response packet received from the host. This function is only
/// called on HOST_REQUEST/HOST_RESPONSE messages.
fn validate_response(header: get_protocol::HeaderHostResponse) -> Result<(), FatalError> {
    // We only support a single header version
    if header.message_version != get_protocol::MessageVersions::HEADER_VERSION_1 {
        return Err(FatalError::InvalidResponseVersion(header.message_version));
    }

    // Response message type must match what's expected
    if header.message_type != get_protocol::MessageTypes::HOST_RESPONSE {
        return Err(FatalError::InvalidResponseType(header.message_type));
    }

    Ok(())
}

/// Parse the HostResponse message sent from the host in `buf`.
fn read_host_response_validated<T: FromBytes + Immutable + KnownLayout>(
    buf: &[u8],
) -> Result<T, FatalError> {
    let response = T::read_from_bytes(buf).map_err(|_| FatalError::MessageSizeHostResponse {
        len: buf.len(),
        response: get_protocol::HeaderHostResponse::read_from_bytes(buf)
            .unwrap()
            .message_id(),
    })?;

    Ok(response)
}

/// Parse the GuestNotification message sent from the host in `buf`.
fn read_guest_notification<T: FromBytes + Immutable + KnownLayout>(
    notification: get_protocol::GuestNotifications,
    buf: &[u8],
) -> Result<T, FatalError> {
    T::read_from_bytes(buf).map_err(|_| FatalError::MessageSizeGuestNotification {
        len: buf.len(),
        notification,
    })
}

/// Defines which host requests should be sent through the secondary host
/// request queue, meaning that their handling on the host may reasonably
/// require interleaving with host requests in the primary queue, or might
/// just be generally slow and shouldn't disrupt the primary queue making
/// forward progress.
fn is_secondary_host_request(request: HostRequests) -> bool {
    request == HostRequests::IGVM_ATTEST || request == HostRequests::VPCI_DEVICE_CONTROL
}

pub(crate) mod msg {
    use crate::api::GuestSaveRequest;
    use crate::client::ModifyVtl2SettingsRequest;
    use chipset_resources::battery::HostBatteryUpdate;
    use guid::Guid;
    use mesh::MeshPayload;
    use mesh::rpc::Rpc;
    use std::sync::Arc;
    use user_driver::DmaClient;
    use vmcore::local_only::LocalOnly;
    use vpci::bus_control::VpciBusEvent;
    use zerocopy::FromBytes;
    use zerocopy::Immutable;
    use zerocopy::IntoBytes;
    use zerocopy::KnownLayout;

    #[derive(Debug, MeshPayload)]
    pub struct VpciListenerRegistrationInput {
        pub bus_instance_id: Guid,
        pub sender: mesh::Sender<VpciBusEvent>,
    }

    /// Necessary data passed via the client to create the `IGVM_ATTEST` request.
    #[derive(Debug, MeshPayload)]
    pub(crate) struct IgvmAttestRequestData {
        pub(crate) agent_data: Vec<u8>,
        pub(crate) report: Vec<u8>,
        pub(crate) response_buffer_len: usize,
    }

    #[derive(Debug, mesh::payload::Protobuf, IntoBytes, FromBytes, Immutable, KnownLayout)]
    #[mesh(bound = "")]
    #[repr(transparent)]
    pub(crate) struct Protocol<T: IntoBytes + FromBytes + Immutable + KnownLayout>(
        #[mesh(encoding = "mesh::payload::encoding::ZeroCopyEncoding")] pub T,
    );

    impl<T> From<T> for Protocol<T>
    where
        T: IntoBytes + FromBytes + Immutable + KnownLayout,
    {
        fn from(value: T) -> Self {
            Protocol(value)
        }
    }

    /// A list specifying control messages to send to the process loop.
    #[derive(MeshPayload)]
    pub(crate) enum Msg {
        // GET infrastructure - not part of the GET protocol itself.
        // No direct interaction with the host.
        /// Instruct the process loop to flush all pending writes to the host.
        ///
        /// The RPC will only be completed once all writes have been sent.
        ///
        /// Note that this only flushes pending message writes, not pending requests.
        FlushWrites(Rpc<(), ()>),
        /// Inspect the state of the process loop.
        Inspect(inspect::Deferred),
        /// Store the gpa allocator to be used for attestation.
        // TODO: Consider a strategy that avoids LocalOnly here.
        SetGpaAllocator(LocalOnly<Arc<dyn DmaClient>>),
        /// Store the callback to trigger the debug interrupt.
        // TODO: Consider a strategy that avoids LocalOnly here.
        SetDebugInterruptCallback(LocalOnly<Box<dyn Fn(u8) + Send + Sync>>),
        /// Notify that live migration has finished.
        SetPostLiveMigrationCallback(LocalOnly<Box<dyn Fn() + Send + Sync>>),

        // Late bound receivers for Guest Notifications
        /// Take the late-bound GuestRequest receiver for Generation Id updates.
        ///
        /// Generation Ids are 128 bit values that the guest can use as entropy
        /// or salt in cryptographic operations. The host updates it when time travel
        /// (i.e. a snapshot restore) is applied, in order for the guest to protect
        /// itself from accidentally replaying operations.
        /// See https://learn.microsoft.com/en-us/windows/win32/hyperv_v2/virtual-machine-generation-identifier
        /// for more information.
        ///
        /// CVM NOTE: CVMs do not yet support restore, so a malicious host choosing
        /// to not update the generation id is not a concern. It is also expected
        /// that the guest will mix the Generation Id with other entropy sources, so
        /// any sort of knowledge-based attack by the host is unlikely.
        TakeGenIdReceiver(Rpc<(), Option<mesh::Receiver<[u8; 16]>>>),
        /// Take the late-bound GuestRequest receiver for servicing save requests.
        ///
        /// CVM NOTE: Servicing is not yet supported, any requests will be rejected.
        TakeSaveRequestReceiver(Rpc<(), Option<mesh::Receiver<GuestSaveRequest>>>),
        /// Take the late-bound receiver for updating dynamic Vtl2 settings.
        /// This is used to inform Underhill of changes to attached storage and
        /// networking devices and their settings.
        ///
        /// CVM NOTE: A malicious host could remove expected devices leading to a DOS,
        /// modify their settings, leading to a DOS or impacted performance,
        /// or add unexpected devices. None of these are a confidentiality concern.
        /// Underhill and the guest must always be careful of how they talk to devices,
        /// regardless of the host's actions.
        ///
        // TODO: Consider a strategy that avoids LocalOnly here.
        TakeVtl2SettingsReceiver(
            Rpc<(), LocalOnly<Option<mesh::Receiver<ModifyVtl2SettingsRequest>>>>,
        ),
        /// Take the late-bound receiver for battery status updates.
        TakeBatteryStatusReceiver(Rpc<(), Option<mesh::Receiver<HostBatteryUpdate>>>),
        /// Register a new VPCI bus event listener with the process loop.
        ///
        /// VPCI bus events are purely informative, no information is sent back to the host.
        // CVM TODO
        VpciListenerRegistration(Rpc<VpciListenerRegistrationInput, ()>),
        /// Deregister a VPCI bus event listener with the process loop.
        VpciListenerDeregistration(Guid),

        // Host Requests
        /// Inform the host that VTL 0 startup is done, and whether it was successful or not.
        CompleteStartVtl0(Rpc<Option<String>, ()>),
        /// Invoke `IVmGuestMemoryAccess::CreateRamGpaRange` on the host.
        ///
        /// See comments in underhill_core/src/emuplat/i440bx_host_pci_bridge.rs for more information.
        CreateRamGpaRange(
            Rpc<CreateRamGpaRangeInput, Protocol<get_protocol::CreateRamGpaRangeResponse>>,
        ),
        /// Get the device platform settings from the host.
        ///
        /// CVM NOTE: The returned data must be validated and/or attested to.
        DevicePlatformSettingsV2(Rpc<(), Vec<u8>>),
        /// Gets the saved state from the host.
        ///
        /// CVM NOTE: CVMs do not support restore, this method should either not
        /// be called or its returned data should be ignored.
        GetVtl2SavedStateFromHost(Rpc<(), Result<Vec<u8>, ()>>),
        /// Get Guest State Protection information.
        GuestStateProtection(
            Rpc<
                Box<Protocol<get_protocol::GuestStateProtectionRequest>>,
                Protocol<get_protocol::GuestStateProtectionResponse>,
            >,
        ),
        /// Get Guest State Protection information, alternate protocol.
        GuestStateProtectionById(Rpc<(), Protocol<get_protocol::GuestStateProtectionByIdResponse>>),
        /// Retrieve the current time.
        ///
        /// CVM NOTE: The returned value should not be relied on for security purposes.
        /// It is expected that the guest will use NTP (or some other time source) after boot.
        HostTime(Rpc<(), Protocol<get_protocol::TimeResponse>>),
        /// Send an attestation request.
        IgvmAttest(Rpc<Box<IgvmAttestRequestData>, Result<Vec<u8>, crate::error::IgvmAttestError>>),
        /// Tell the host the location of the framebuffer.
        MapFramebuffer(Rpc<u64, Protocol<get_protocol::MapFramebufferResponse>>),
        /// Reset and unmap a memory range previously created by CreateRamGpaRange.
        ResetRamGpaRange(Rpc<u32, Protocol<get_protocol::ResetRamGpaRangeResponse>>),
        /// Send saved state (or an error message) to the host so it can be used to start
        /// a new VM after servicing.
        SendServicingState(Rpc<Result<Vec<u8>, String>, Result<(), ()>>),
        /// Tell the host to unmap the framebuffer.
        UnmapFramebuffer(Rpc<(), Protocol<get_protocol::UnmapFramebufferResponse>>),
        /// Read a PCI config space value from the proxied VGA device.
        VgaProxyPciRead(Rpc<u16, Protocol<get_protocol::VgaProxyPciReadResponse>>),
        /// Write a PCI config space value to the proxied VGA device.
        VgaProxyPciWrite(
            Rpc<VgaProxyPciWriteInput, Protocol<get_protocol::VgaProxyPciWriteResponse>>,
        ),
        /// Flush any pending writes to the VMGS.
        VmgsFlush(Rpc<(), Protocol<get_protocol::VmgsFlushResponse>>),
        /// Get basic metadata about the VMGS.
        VmgsGetDeviceInfo(Rpc<(), Protocol<get_protocol::VmgsGetDeviceInfoResponse>>),
        /// Read from the VMGS.
        ///
        /// CVM NOTE: CVMs encrypt their VMGS and this data transfer is done at a raw block level.
        /// The host will only ever see encrypted data.
        VmgsRead(Rpc<VmgsReadInput, Result<Vec<u8>, Protocol<get_protocol::VmgsReadResponse>>>),
        /// Write to the VMGS.
        ///
        /// CVM NOTE: CVMs encrypt their VMGS and this data transfer is done at a raw block level.
        /// The host will only ever see encrypted data.
        VmgsWrite(Rpc<VmgsWriteInput, Result<(), Protocol<get_protocol::VmgsWriteResponse>>>),
        // CVM TODO
        VpciDeviceBindingChange(
            Rpc<
                VpciDeviceBindingChangeInput,
                Protocol<get_protocol::VpciDeviceBindingChangeResponse>,
            >,
        ),
        // CVM TODO
        VpciDeviceControl(
            Rpc<VpciDeviceControlInput, Protocol<get_protocol::VpciDeviceControlResponse>>,
        ),

        // Host Notifications (don't require a response)
        /// Report an event to the host.
        EventLog(Protocol<get_protocol::EventLogId>),
        /// Report a power state change to the host.
        PowerState(PowerState),
        /// Report the result of a restore operation to the host.
        ReportRestoreResultToHost(bool),
        /// Report a VP triple fault to the host.
        TripleFaultNotification(Vec<u8>),
        /// Report a guest crash to the host.
        VtlCrashNotification(Protocol<get_protocol::VtlCrashNotification>),
    }

    #[derive(Debug, MeshPayload)]
    pub enum PowerState {
        PowerOff,
        Reset,
        Hibernate,
    }

    #[derive(Debug, MeshPayload)]
    pub struct VmgsReadInput {
        pub sector_offset: u64,
        pub sector_count: u32,
        pub sector_size: u32,
    }

    #[derive(Debug, MeshPayload)]
    pub struct VmgsWriteInput {
        pub sector_offset: u64,
        pub buf: Vec<u8>,
        pub sector_size: u32,
    }

    #[derive(Debug, MeshPayload)]
    pub struct VpciDeviceControlInput {
        pub code: Protocol<get_protocol::VpciDeviceControlCode>,
        pub bus_instance_id: Guid,
    }

    #[derive(Debug, MeshPayload)]
    pub struct VpciDeviceBindingChangeInput {
        pub bus_instance_id: Guid,
        pub binding_state: bool,
    }

    #[derive(Debug, MeshPayload)]
    pub struct VgaProxyPciWriteInput {
        pub offset: u16,
        pub value: u32,
    }

    #[derive(Debug, MeshPayload)]
    pub struct CreateRamGpaRangeInput {
        pub slot: u32,
        pub gpa_start: u64,
        pub gpa_count: u64,
        pub gpa_offset: u64,
        pub flags: Protocol<crate::api::CreateRamGpaRangeFlags>,
    }
}

/// A variant of `Option<mesh::Sender<T>>` that buffers a fixed number of
/// outgoing messages if the `mesh::Sender<T>` hasn't been connected.
#[derive(Inspect)]
#[inspect(external_tag)]
enum BufferedSender<const MAX_SIZE: usize, T> {
    Buffered(#[inspect(rename = "len", with = "Vec::len")] Vec<T>),
    Ready(#[inspect(skip)] mesh::Sender<T>),
}

struct BufferedSenderFull;

impl<const MAX_SIZE: usize, T: Send + 'static> BufferedSender<MAX_SIZE, T> {
    fn new() -> Self {
        BufferedSender::Buffered(Vec::new())
    }

    /// Initializes the receiver, and reports the number of buffered messages
    /// that were immediately flushed.
    ///
    /// Returns `None` if the receiver has already been initialized.
    fn init_receiver(&mut self) -> Option<(mesh::Receiver<T>, usize)> {
        match self {
            BufferedSender::Buffered(buf) => {
                let buf = std::mem::take(buf);
                let (send, recv) = mesh::channel();
                let n = buf.len();
                for msg in buf {
                    send.send(msg)
                }
                *self = BufferedSender::Ready(send);
                Some((recv, n))
            }
            BufferedSender::Ready(_) => None,
        }
    }

    /// Just like [`mesh::Sender::send`], albeit returning an error if the
    /// fixed-size buffer has overflowed.
    fn send(&mut self, msg: T) -> Result<(), BufferedSenderFull> {
        match self {
            BufferedSender::Buffered(buf) => {
                if buf.len() == MAX_SIZE {
                    return Err(BufferedSenderFull);
                }
                buf.push(msg);
            }
            BufferedSender::Ready(sender) => {
                sender.send(msg);
            }
        }

        Ok(())
    }
}

impl<const MAX_SIZE: usize, T: Send + 'static> TryRpcSend for &mut BufferedSender<MAX_SIZE, T> {
    type Message = T;
    type Error = BufferedSenderFull;

    fn try_send_rpc(self, message: Self::Message) -> Result<(), Self::Error> {
        self.send(message)
    }
}

/// A variant of `Option<mesh::Sender<T>>` for late-bound guest notification
/// consumers that buffers a fixed-number of messages during the window between
/// GET init and worker startup.
///
/// # Why is this type necessary?
///
/// The GET's current protocol doesn't afford any mechanism for the Guest to
/// signal to the Host when it's ready to begin processing Guest Notifications.
///
/// In other words: from the moment that the GET has been initialized, it
/// becomes legal (at a protocol-level) for the Host to begin sending Guest
/// Notifications that it expects to be serviced.
///
/// Unfortunately, the workers responsible for handling these notifications only
/// get initialized _after_ the GET (which needs to be brought up very-early in
/// the init process), resulting in a non-trivial window of time where a guest
/// notifications can arrive before it can be properly serviced!
///
/// > Note: this issue was introduced in the NI protocol revision, and not
/// > discovered until _after_ it was too late to change the protocol. As such,
/// > we're stuck with this behavior for the foreseeable future.
///
/// So, how do we work around this?
///
/// Dropping the notifications during this window isn't an option, due to the
/// aforementioned "host expects notifications to be serviced" requirement.
///
/// Eagerly initializing the `mesh` channels as part of GET construction would
/// ensure all notifications get buffered regardless if the receiving end is
/// hooked up yet... but that could result in unbounded buffering if the
/// receiving end never get hooked-up, resulting in hard-to-debug OOM issues.
///
/// The only reasonable option (as implemented on by type) is to buffer a
/// fixed-number of notifications that arrive during this window, fast-failing
/// if too many notifications arrive (which would indicate a logic error on the
/// host-side, or within underhill init).
// DEVNOTE: the choice of limit here is entirely arbitrary. not too high as to
// mask serious issues, and not too low as to fail in exceptional circumstances.
type GuestNotificationSender<T> = BufferedSender<16, T>;

fn log_buffered_guest_notifications<T>(
    kind: get_protocol::GuestNotifications,
) -> impl FnOnce((mesh::Receiver<T>, usize)) -> mesh::Receiver<T> {
    move |(recv, flushed)| {
        if flushed > 0 {
            tracing::info!(?kind, flushed, "flushing buffered guest notifications")
        }
        recv
    }
}

#[derive(InspectMut)]
pub(crate) struct ProcessLoop<T: RingMem> {
    #[inspect(mut)]
    pipe: MessagePipe<T>,
    #[inspect(skip)]
    vtl2_settings_buf: Option<Vec<u8>>,
    #[inspect(skip)]
    primary_host_requests: HostRequestQueue,
    #[inspect(skip)]
    pipe_channels: PipeChannels,
    #[inspect(skip)]
    read_send: mesh::Sender<Vec<u8>>,
    #[inspect(skip)]
    write_recv: mesh::Receiver<WriteRequest>,
    #[inspect(skip)]
    secondary_host_requests: HostRequestQueue,
    #[inspect(skip)]
    secondary_host_requests_read_send: mesh::Sender<Vec<u8>>,
    gpa_allocator: Option<Arc<dyn DmaClient>>,
    #[inspect(skip)]
    set_debug_interrupt: Option<Box<dyn Fn(u8) + Send + Sync>>,
    #[inspect(skip)]
    post_live_migration: Option<Box<dyn Fn() + Send + Sync>>,
    stats: Stats,

    guest_notification_listeners: GuestNotificationListeners,
    #[inspect(skip)]
    guest_notification_responses:
        FuturesUnordered<Pin<Box<dyn Send + Future<Output = GuestNotificationResponse>>>>,
}

// Outbound channels that relay Guest Notifications to code outside the core GET
// worker.
#[derive(Inspect)]
struct GuestNotificationListeners {
    generation_id: GuestNotificationSender<[u8; 16]>,
    save_request: GuestNotificationSender<GuestSaveRequest>,
    vtl2_settings: GuestNotificationSender<ModifyVtl2SettingsRequest>,
    #[inspect(skip)]
    vpci: HashMap<Guid, mesh::Sender<VpciBusEvent>>,
    battery_status: GuestNotificationSender<HostBatteryUpdate>,
}

// DEVNOTE: The fact that we even have a notion of "guest notification
// responses" is indicative of a design blunder, given that the whole point of
// having "notifications" at the protocol level was that notifications aren't
// supposed to need responses! i.e: notifications (both host and guest) ought to
// be "fire and forget" operations.
//
// Unfortunately, the MODIFY_VTL2_SETTINGS guest notification and
// MODIFY_VTL2_SETTINGS_COMPLETE host notification don't follow this design
// principal, and by the time we realized the design issue, it was too late to
// fix the protocol.
//
// In hindsight, MODIFY_VTL2_SETTINGS flow really ought to have been modeled as
// some kind of `GuestRequest`/`GuestResponse` pair at the protocol level,
// rather than "abusing" guest/host notifications.
//
// Alas, that's not how the cookie crumbled, and as a result, we're stuck with a
// pair of notifications that don't really act like "notifications" for the
// foreseeable future...
enum GuestNotificationResponse {
    ModifyVtl2Settings(Result<(), RpcError<Vec<Vtl2SettingsErrorInfo>>>),
}

#[derive(Default, Inspect)]
struct Stats {
    #[inspect(with = "inspect_helpers::iter_by_debug_key")]
    host_requests: HashMap<HostRequests, Counter>,
    #[inspect(with = "inspect_helpers::iter_by_debug_key")]
    host_responses: HashMap<HostRequests, Counter>,
    #[inspect(with = "inspect_helpers::iter_by_debug_key")]
    host_notifications: HashMap<get_protocol::HostNotifications, Counter>,
    #[inspect(with = "inspect_helpers::iter_by_debug_key")]
    guest_notifications: HashMap<get_protocol::GuestNotifications, Counter>,
}

mod inspect_helpers {
    use super::*;

    pub fn iter_by_debug_key<T: core::fmt::Debug>(map: &HashMap<T, Counter>) -> impl Inspect + '_ {
        inspect::iter_by_key(map).map_key(|x| format!("{:?}", x))
    }
}

struct HostRequestPipeAccess {
    response_message_recv_mutex: ExclusiveHostRequestResponseReceiverAccess,
    response_message_recv: Option<mesh::Receiver<Vec<u8>>>,
    request_message_send: mesh::Sender<WriteRequest>,
}

impl Drop for HostRequestPipeAccess {
    fn drop(&mut self) {
        *self.response_message_recv_mutex.lock() = Some(self.response_message_recv.take().unwrap());
    }
}

struct PipeChannels {
    // This is None when a `HostRequestPipeAccess` has ownership for a host
    // request in the primary queue.
    primary_host_request_response_message_recv: ExclusiveHostRequestResponseReceiverAccess,
    // This is None when a `HostRequestPipeAccess` has ownership for a host
    // request in the secondary queue.
    secondary_host_request_response_message_recv: ExclusiveHostRequestResponseReceiverAccess,
    message_send: mesh::Sender<WriteRequest>,
}

enum WriteRequest {
    Message(Vec<u8>),
    Flush(Rpc<(), ()>),
}

impl HostRequestPipeAccess {
    fn new(
        response_message_recv_mutex: Arc<Mutex<Option<mesh::Receiver<Vec<u8>>>>>,
        request_message_send: mesh::Sender<WriteRequest>,
    ) -> Self {
        let response_message_recv = response_message_recv_mutex.lock().take().unwrap();
        Self {
            response_message_recv_mutex,
            response_message_recv: Some(response_message_recv),
            request_message_send,
        }
    }

    /// Sends a message to the host.
    fn send_message(&mut self, message: Vec<u8>) {
        self.request_message_send
            .send(WriteRequest::Message(message));
    }

    /// Waits for a response message from the host.
    ///
    /// The caller is responsible for validating the message ID.
    async fn recv_response(&mut self) -> Vec<u8> {
        self.response_message_recv
            .as_mut()
            .unwrap()
            .recv()
            .await
            .unwrap()
    }

    /// Waits for a known, fixed-size response message from the host.
    ///
    /// The caller is responsible for validating the message ID.
    async fn recv_response_fixed_size<T: IntoBytes + FromBytes + Immutable + KnownLayout>(
        &mut self,
        id: HostRequests,
    ) -> Result<T, FatalError> {
        let response = self.recv_response().await;
        let header = get_protocol::HeaderHostRequest::read_from_prefix(response.as_bytes())
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        if id != header.message_id() {
            return Err(FatalError::ResponseHeaderMismatchId(
                header.message_id(),
                id,
            ));
        }
        read_host_response_validated(&response)
    }

    /// Sends a request to the host and waits for a fixed-size response.
    ///
    /// Fails if the response's message ID does not match the request's message
    /// ID.
    async fn send_request_fixed_size<
        T: IntoBytes + ?Sized + Immutable + KnownLayout,
        U: IntoBytes + FromBytes + Immutable + KnownLayout,
    >(
        &mut self,
        data: &T,
    ) -> Result<U, FatalError> {
        self.send_message(data.as_bytes().to_vec());
        let req_header = get_protocol::HeaderHostRequest::read_from_prefix(data.as_bytes())
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        self.recv_response_fixed_size(req_header.message_id()).await
    }

    /// Sends a fail notification to the host.
    ///
    /// This function does not wait for a response from the host.
    /// It is specifically designed for scenarios where the host does not send any response.
    /// One of such scenario is the save failure, where host does not send any response.
    ///
    /// In the future, GED notifications for failures need to be added.
    /// This will require updates to both the host and openHCL.
    async fn send_failed_save_state<T: IntoBytes + ?Sized + Immutable + KnownLayout>(
        &mut self,
        data: &T,
    ) -> Result<(), FatalError> {
        self.send_message(data.as_bytes().to_vec());
        Ok(())
    }
}

impl<T: RingMem> ProcessLoop<T> {
    pub(crate) fn new(pipe: MessagePipe<T>) -> Self {
        let (read_send, read_recv) = mesh::channel();
        let (secondary_host_requests_read_send, secondary_host_requests_read_recv) =
            mesh::channel();
        let (write_send, write_recv) = mesh::channel();

        Self {
            pipe,
            stats: Default::default(),
            guest_notification_responses: Default::default(),
            vtl2_settings_buf: None,
            primary_host_requests: Default::default(),
            secondary_host_requests: Default::default(),
            pipe_channels: PipeChannels {
                primary_host_request_response_message_recv: Arc::new(Mutex::new(Some(read_recv))),
                secondary_host_request_response_message_recv: Arc::new(Mutex::new(Some(
                    secondary_host_requests_read_recv,
                ))),
                message_send: write_send,
            },
            read_send,
            write_recv,
            secondary_host_requests_read_send,
            guest_notification_listeners: GuestNotificationListeners {
                generation_id: GuestNotificationSender::new(),
                vtl2_settings: GuestNotificationSender::new(),
                save_request: GuestNotificationSender::new(),
                vpci: HashMap::new(),
                battery_status: GuestNotificationSender::new(),
            },
            gpa_allocator: None,
            set_debug_interrupt: None,
            post_live_migration: None,
        }
    }

    /// Write to the file descriptor the whole contents of byte stream Vec.
    fn send_message(&mut self, buf: Vec<u8>) {
        self.pipe_channels
            .message_send
            .send(WriteRequest::Message(buf));
    }

    /// Read a message from the pipe.
    async fn read_pipe(&mut self, buf: &mut [u8]) -> Result<usize, FatalError> {
        self.pipe.recv(buf).await.map_err(FatalError::FdIo)
    }

    /// Negotiate the protocol version with the host.
    pub(crate) async fn negotiate_version(
        &mut self,
    ) -> Result<get_protocol::ProtocolVersion, FatalError> {
        // This function doesn't follow the ProcessLoopMessage protocol because
        // self.run() isn't running yet to take requests, so we must send requests
        // manually.

        // Negotiate the protocol.
        for protocol in [get_protocol::ProtocolVersion::NICKEL_REV2] {
            let version_request = get_protocol::VersionRequest::new(protocol);

            self.pipe
                .send(version_request.as_bytes())
                .await
                .map_err(FatalError::FdIo)?;

            // The next message must be the version response.
            let mut response = get_protocol::VersionResponse::new_zeroed();
            let len = self.read_pipe(response.as_mut_bytes()).await?;

            validate_response(response.message_header)?;

            if len != response.as_bytes().len() {
                return Err(FatalError::MessageSizeHostResponse {
                    len,
                    response: HostRequests::VERSION,
                });
            }

            if response.message_header.message_id() != version_request.message_header.message_id() {
                return Err(FatalError::ResponseHeaderMismatchId(
                    response.message_header.message_id(),
                    version_request.message_header.message_id(),
                ));
            }

            let version_accepted: bool = response
                .version_accepted
                .into_bool()
                .map_err(|_| FatalError::InvalidResponse)?;

            if version_accepted {
                tracing::info!(?protocol, "[GET] version negotiated");

                return Ok(protocol);
            }
        }

        Err(FatalError::VersionNegotiationFailed)
    }

    /// Run the protocol handling loop.
    pub(crate) async fn run(&mut self, mut recv: mesh::Receiver<Msg>) -> Result<(), FatalError> {
        let mut buf: Box<[u8; get_protocol::MAX_MESSAGE_SIZE]> =
            Box::new([0; get_protocol::MAX_MESSAGE_SIZE]);

        let mut outgoing = Vec::new();
        loop {
            enum Event {
                Msg(Msg),
                Done,
                Header(Result<usize, FatalError>),
                GuestNotificationResponse(GuestNotificationResponse),
                Failure(FatalError),
            }

            let event = {
                let (mut read, mut write) = self.pipe.split();

                // Read the next incoming message.
                let read_fd = read
                    .recv(buf.as_mut())
                    .map_err(FatalError::FdIo)
                    .map(Event::Header);

                // Read the next request.
                let recv_msg = recv.recv().map(|r| r.map_or(Event::Done, Event::Msg));

                // Write any pending outgoing message.
                let send_next = async {
                    loop {
                        if !outgoing.is_empty() {
                            // With the current GET infrastructure, the easiest
                            // way to get these kinds of per-message stats is to
                            // quickly re-parse the header before sending the
                            // message down the wire.
                            if let Ok((header, _)) =
                                get_protocol::HeaderRaw::read_from_prefix(outgoing.as_ref())
                            // TODO: zerocopy: use-rest-of-range, zerocopy: err (https://github.com/microsoft/openvmm/issues/759)
                            {
                                match header.message_type {
                                    get_protocol::MessageTypes::HOST_REQUEST => {
                                        (self.stats.host_requests)
                                            .entry(HostRequests(header.message_id))
                                            .or_default()
                                            .increment();
                                    }
                                    get_protocol::MessageTypes::HOST_NOTIFICATION => {
                                        (self.stats.host_notifications)
                                            .entry(get_protocol::HostNotifications(
                                                header.message_id,
                                            ))
                                            .or_default()
                                            .increment();
                                    }
                                    _ => {}
                                }
                            }

                            if let Err(err) = write.send(&outgoing).await {
                                return FatalError::FdIo(err);
                            }
                            outgoing.clear();
                        }
                        match self.write_recv.recv().await.unwrap() {
                            WriteRequest::Message(message) => outgoing = message,
                            WriteRequest::Flush(send) => send.complete(()),
                        }
                    }
                }
                .map(Event::Failure);

                // Closure to share code between the primary and secondary host request queue.
                let run_host_request_queue =
                    async |request_queue: &mut HostRequestQueue, response_recv: &ExclusiveHostRequestResponseReceiverAccess| {
                        while let Some(request) = request_queue.front_mut() {
                            if let Err(e) = request.as_mut().await {
                                return e;
                            }

                            request_queue.pop_front();

                            // Ensure there are no extra response messages that this request failed to pick up.
                            if let Ok(resp) = response_recv.lock().as_mut().unwrap().try_recv() {
                                let id = get_protocol::HeaderRaw::read_from_prefix(&resp)
                                    .map(|head| HostRequests(head.0.message_id))
                                    .unwrap_or(HostRequests::INVALID);
                                tracing::warn!(?id, "received extra response after processing request");
                            }
                        }
                        pending().await
                    };

                // Run the next host request in the primary queue. These host
                // requests are expected to be generally fast and independent
                // of one another.
                let run_next_primary_host_request = run_host_request_queue(
                    &mut self.primary_host_requests,
                    &self
                        .pipe_channels
                        .primary_host_request_response_message_recv,
                )
                .map(Event::Failure);

                // Run the next host request in the secondary queue. These
                // host requests may need to interleave with requests in the
                // primary queue, or may be generally slow and shouldn't
                // disrupt forward progress in the primary queue.
                let run_next_secondary_host_request = run_host_request_queue(
                    &mut self.secondary_host_requests,
                    &self
                        .pipe_channels
                        .secondary_host_request_response_message_recv,
                )
                .map(Event::Failure);

                let recv_response = async {
                    if self.guest_notification_responses.is_empty() {
                        pending().await
                    } else {
                        Event::GuestNotificationResponse(
                            self.guest_notification_responses.next().await.unwrap(),
                        )
                    }
                };

                (
                    read_fd,
                    recv_msg,
                    send_next,
                    run_next_primary_host_request,
                    run_next_secondary_host_request,
                    recv_response,
                )
                    .race()
                    .await
            };

            // Note that events are all handled synchronously. This ensures that
            // additional notifications or responses are not blocked behind
            // handling an existing one.
            match event {
                Event::Done => break Ok(()),
                Event::Failure(err) => return Err(err),
                Event::Msg(message) => {
                    self.process_host_request(message)?;
                }
                Event::Header(len) => {
                    let len = len?;
                    let buf = &buf[..len];
                    let header = get_protocol::HeaderRaw::read_from_prefix(buf)
                        .map_err(|_| FatalError::MessageSizeHeader(len))?
                        .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

                    match header.message_type {
                        get_protocol::MessageTypes::HOST_RESPONSE => {
                            (self.stats.host_responses)
                                .entry(HostRequests(header.message_id))
                                .or_default()
                                .increment();

                            self.handle_host_response(
                                header.try_into().expect("validated message type"),
                                buf,
                            )?;
                        }
                        get_protocol::MessageTypes::GUEST_NOTIFICATION => {
                            (self.stats.guest_notifications)
                                .entry(get_protocol::GuestNotifications(header.message_id))
                                .or_default()
                                .increment();

                            self.handle_guest_notification(
                                header.try_into().expect("validated message type"),
                                buf,
                            )?;
                        }
                        _ => panic!("Unexpected header type received: {:?}!", header),
                    }
                }
                Event::GuestNotificationResponse(response) => match response {
                    GuestNotificationResponse::ModifyVtl2Settings(response) => {
                        self.complete_modify_vtl2_settings(response)?
                    }
                },
            }
        }
    }

    /// Spawn a host request in the primary queue.
    ///
    /// `f` will receive a [`PipeAccess`] to send messages to the host and
    /// receive host responses.
    ///
    /// The result of `f().await` will be sent as a response to the RPC in `req`.
    fn push_primary_host_request_handler<F, Fut>(&mut self, f: F)
    where
        F: 'static + Send + FnOnce(HostRequestPipeAccess) -> Fut,
        Fut: 'static + Future<Output = Result<(), FatalError>> + Send,
    {
        let message_recv_mutex = self
            .pipe_channels
            .primary_host_request_response_message_recv
            .clone();
        let message_send = self.pipe_channels.message_send.clone();
        let fut = async { f(HostRequestPipeAccess::new(message_recv_mutex, message_send)).await };
        self.primary_host_requests.push_back(Box::pin(fut));
    }

    /// Spawn host request on the secondary queue.
    ///
    /// `f` will receive a [`PipeAccess`] to send messages to the host and
    /// receive host responses.
    ///
    /// The result of `f().await` will be sent as a response to the RPC in `req`.
    fn push_secondary_host_request_handler<F, Fut>(&mut self, f: F)
    where
        F: 'static + Send + FnOnce(HostRequestPipeAccess) -> Fut,
        Fut: 'static + Future<Output = Result<(), FatalError>> + Send,
    {
        let message_recv_mutex = self
            .pipe_channels
            .secondary_host_request_response_message_recv
            .clone();
        let message_send = self.pipe_channels.message_send.clone();
        let fut = async { f(HostRequestPipeAccess::new(message_recv_mutex, message_send)).await };
        self.secondary_host_requests.push_back(Box::pin(fut));
    }

    /// Pushes a host request handler that sends a single host request on the
    /// primary queue and waits for its response.
    fn push_basic_host_request_handler<Req, I, Resp>(
        &mut self,
        req: Rpc<I, Resp>,
        f: impl 'static + Send + FnOnce(I) -> Req,
    ) where
        Req: IntoBytes + 'static + Send + Sync + Immutable + KnownLayout,
        I: 'static + Send,
        Resp: 'static + IntoBytes + FromBytes + Send + Immutable + KnownLayout,
    {
        self.push_primary_host_request_handler(async move |mut access| {
            req.handle_must_succeed(async |input| access.send_request_fixed_size(&f(input)).await)
                .await
        });
    }

    /// Process a host request message sent from the guest to the host, or an infra
    /// request that gets handled fully locally.
    fn process_host_request(&mut self, message: Msg) -> Result<(), FatalError> {
        match message {
            // GET infrastructure - not part of the GET protocol itself.
            // No direct interaction with the host.
            Msg::FlushWrites(rpc) => {
                self.pipe_channels
                    .message_send
                    .send(WriteRequest::Flush(rpc));
            }
            Msg::Inspect(req) => {
                req.inspect(self);
            }
            Msg::SetGpaAllocator(gpa_allocator) => {
                self.gpa_allocator = Some(gpa_allocator.0);
            }
            Msg::SetDebugInterruptCallback(callback) => {
                self.set_debug_interrupt = Some(callback.0);
            }
            Msg::SetPostLiveMigrationCallback(callback) => {
                self.post_live_migration = Some(callback.0);
            }

            // Late bound receivers for Guest Notifications
            Msg::TakeVtl2SettingsReceiver(req) => req.handle_sync(|()| {
                vmcore::local_only::LocalOnly(
                    self.guest_notification_listeners
                        .vtl2_settings
                        .init_receiver()
                        .map(log_buffered_guest_notifications(
                            get_protocol::GuestNotifications::MODIFY_VTL2_SETTINGS,
                        )),
                )
            }),
            Msg::TakeGenIdReceiver(req) => req.handle_sync(|()| {
                self.guest_notification_listeners
                    .generation_id
                    .init_receiver()
                    .map(log_buffered_guest_notifications(
                        get_protocol::GuestNotifications::UPDATE_GENERATION_ID,
                    ))
            }),
            Msg::TakeSaveRequestReceiver(req) => req.handle_sync(|()| {
                self.guest_notification_listeners
                    .save_request
                    .init_receiver()
                    .map(log_buffered_guest_notifications(
                        get_protocol::GuestNotifications::SAVE_GUEST_VTL2_STATE,
                    ))
            }),
            Msg::TakeBatteryStatusReceiver(req) => req.handle_sync(|()| {
                self.guest_notification_listeners
                    .battery_status
                    .init_receiver()
                    .map(log_buffered_guest_notifications(
                        get_protocol::GuestNotifications::BATTERY_STATUS,
                    ))
            }),
            Msg::VpciListenerRegistration(req) => {
                req.handle_sync(|input| {
                    self.guest_notification_listeners
                        .vpci
                        .insert(input.bus_instance_id, input.sender);
                });
            }
            Msg::VpciListenerDeregistration(bus_instance_id) => {
                self.guest_notification_listeners
                    .vpci
                    .remove(&bus_instance_id);
            }

            // Host Requests
            Msg::DevicePlatformSettingsV2(req) => {
                self.push_primary_host_request_handler(|access| {
                    req.handle_must_succeed(async |()| {
                        request_device_platform_settings_v2(access).await
                    })
                });
            }
            Msg::VmgsFlush(req) => {
                self.push_basic_host_request_handler(req, |()| {
                    get_protocol::VmgsFlushRequest::new()
                });
            }
            Msg::VmgsGetDeviceInfo(req) => {
                self.push_basic_host_request_handler(req, |()| {
                    get_protocol::VmgsGetDeviceInfoRequest::new()
                });
            }
            Msg::GetVtl2SavedStateFromHost(req) => {
                self.push_primary_host_request_handler(|access| {
                    req.handle_must_succeed(async |()| request_saved_state(access).await)
                })
            }
            Msg::GuestStateProtection(req) => {
                self.push_basic_host_request_handler(req, |request| *request);
            }
            Msg::GuestStateProtectionById(req) => {
                self.push_basic_host_request_handler(req, |()| {
                    get_protocol::GuestStateProtectionByIdRequest::new()
                });
            }
            Msg::HostTime(req) => {
                self.push_basic_host_request_handler(req, |()| get_protocol::TimeRequest::new());
            }
            Msg::IgvmAttest(req) => {
                let shared_pool_allocator = self.gpa_allocator.clone();

                self.push_secondary_host_request_handler(|access| {
                    req.handle_must_succeed(async |request| {
                        request_igvm_attest(access, *request, shared_pool_allocator).await
                    })
                });
            }
            Msg::VmgsRead(req) => {
                self.push_primary_host_request_handler(|access| {
                    req.handle_must_succeed(async |input| {
                        request_vmgs_read(access, input)
                            .await
                            .map(|x| x.map_err(Into::into))
                    })
                });
            }
            Msg::VmgsWrite(req) => {
                self.push_primary_host_request_handler(|access| {
                    req.handle_must_succeed(async |input| {
                        request_vmgs_write(access, input)
                            .await
                            .map(|x| x.map_err(Into::into))
                    })
                });
            }
            Msg::VpciDeviceControl(req) => {
                self.push_secondary_host_request_handler(|access| {
                    req.handle_must_succeed(async |input| {
                        request_vpci_device_control(access, input)
                            .await
                            .map(Into::into)
                    })
                });
            }
            Msg::VpciDeviceBindingChange(req) => {
                self.push_basic_host_request_handler(req, |input| {
                    get_protocol::VpciDeviceBindingChangeRequest::new(
                        input.bus_instance_id,
                        input.binding_state,
                    )
                });
            }
            Msg::VgaProxyPciRead(req) => {
                self.push_basic_host_request_handler(req, |input| {
                    get_protocol::VgaProxyPciReadRequest::new(input)
                });
            }
            Msg::VgaProxyPciWrite(req) => {
                self.push_basic_host_request_handler(req, |input| {
                    get_protocol::VgaProxyPciWriteRequest::new(input.offset, input.value)
                });
            }
            Msg::MapFramebuffer(req) => {
                self.push_basic_host_request_handler(req, |input| {
                    get_protocol::MapFramebufferRequest::new(input)
                });
            }
            Msg::UnmapFramebuffer(req) => {
                self.push_basic_host_request_handler(req, |()| {
                    get_protocol::UnmapFramebufferRequest::new()
                });
            }
            Msg::CreateRamGpaRange(req) => {
                self.push_basic_host_request_handler(req, |input| {
                    get_protocol::CreateRamGpaRangeRequest::new(
                        input.slot,
                        input.gpa_start,
                        input.gpa_count,
                        input.gpa_offset,
                        input.flags.0,
                    )
                });
            }
            Msg::ResetRamGpaRange(req) => {
                self.push_basic_host_request_handler(req, |input| {
                    get_protocol::ResetRamGpaRangeRequest::new(input)
                });
            }
            Msg::SendServicingState(req) => self.push_primary_host_request_handler(move |access| {
                req.handle_must_succeed(async |data| {
                    request_send_servicing_state(access, data).await
                })
            }),
            Msg::CompleteStartVtl0(rpc) => {
                let (input, res) = rpc.split();
                self.complete_start_vtl0(input)?;
                res.complete(());
            }

            // Host Notifications (don't require a response)
            Msg::PowerState(state) => {
                // Queue behind any pending requests to avoid terminating while
                // something important is in flight.
                self.push_primary_host_request_handler(async move |mut access| {
                    let message = match state {
                        msg::PowerState::PowerOff => get_protocol::PowerOffNotification::new(false)
                            .as_bytes()
                            .to_vec(),
                        msg::PowerState::Hibernate => get_protocol::PowerOffNotification::new(true)
                            .as_bytes()
                            .to_vec(),
                        msg::PowerState::Reset => {
                            get_protocol::ResetNotification::new().as_bytes().to_vec()
                        }
                    };
                    access.send_message(message);
                    Ok(())
                })
            }
            Msg::EventLog(event_log_id) => {
                // Send the event log right away, jumping the line in front of
                // any pending requests.
                self.send_message(
                    get_protocol::EventLogNotification::new(event_log_id.0)
                        .as_bytes()
                        .to_vec(),
                );
            }
            Msg::ReportRestoreResultToHost(success) => self.report_restore_result_to_host(success),
            Msg::VtlCrashNotification(crash_notification) => {
                // Send the crash notification right away, jumping the line in front of
                // any pending requests.
                // There is no versioning used for this notification. If the host does not
                // support this notification, the host drop it to no ill-effects.
                self.send_message(crash_notification.0.as_bytes().to_vec());
            }
            Msg::TripleFaultNotification(triple_fault_notification) => {
                self.send_message(triple_fault_notification);
            }
        }

        Ok(())
    }

    /// Handle a guest notification header.
    fn handle_guest_notification(
        &mut self,
        header: get_protocol::HeaderGuestNotification,
        buf: &[u8],
    ) -> Result<(), FatalError> {
        use get_protocol::GuestNotifications;

        // Version must be at least version 1.
        if header.message_version < get_protocol::MessageVersions::HEADER_VERSION_1 {
            tracing::error!(
                msg = ?buf,
                version = ?header.message_version,
                "invalid header version in guest notification",
            );
            return Err(FatalError::InvalidGuestNotificationVersion(
                header.message_version,
            ));
        }

        let id = header.message_id();
        match id {
            GuestNotifications::UPDATE_GENERATION_ID => {
                self.handle_update_generation_id(read_guest_notification(id, buf)?)?;
            }
            GuestNotifications::SAVE_GUEST_VTL2_STATE => {
                self.handle_save_state_notification(read_guest_notification(id, buf)?)?;
            }
            GuestNotifications::MODIFY_VTL2_SETTINGS => {
                // Protocol wart: the fact that we still support this packet
                // (even through MODIFY_VTL2_SETTINGS_REV1 exists) is unfortunate.
                //
                // We should've deprecated this packet and just used
                // MODIFY_VTL2_SETTINGS_REV1 for everything... but we didn't,
                // and by the time we realized, it was too late to fix the
                // host-side.
                //
                // Hosts will continue to use this packet whenever the payload
                // is "small", even though MODIFY_VTL2_SETTINGS_REV1 is totally
                // capable of sending small payloads itself...
                self.handle_modify_vtl2_settings_notification(buf)?;
            }
            GuestNotifications::MODIFY_VTL2_SETTINGS_REV1 => {
                self.handle_modify_vtl2_settings_rev1_notification(buf)?;
            }
            GuestNotifications::VPCI_DEVICE_NOTIFICATION => {
                self.handle_vpci_device_notification(read_guest_notification(id, buf)?)?;
            }
            GuestNotifications::BATTERY_STATUS => {
                self.handle_battery_status_notification(read_guest_notification(id, buf)?)?;
            }
            GuestNotifications::INJECT_DEBUG_INTERRUPT => {
                self.handle_debug_interrupt_notification(read_guest_notification(id, buf)?)?;
            }
            GuestNotifications::NOTIFY_POST_LIVE_MIGRATION => {
                self.handle_post_live_migration_notification(read_guest_notification(id, buf)?);
            }
            invalid_notification => {
                tracing::error!(
                    ?invalid_notification,
                    "[HOST GET] ignoring invalid guest notification",
                );
            }
        }

        Ok(())
    }

    /// Reads the host response and validates response matches the expected
    /// request sent to the host, before sending the data back to the client. All
    /// errors encountered are sent back to the client, except transport errors
    /// between process loop and client and unwarranted/incorrect HostResponses
    fn handle_host_response(
        &mut self,
        header: get_protocol::HeaderHostResponse,
        buf: &[u8],
    ) -> Result<(), FatalError> {
        validate_response(header)?;

        if is_secondary_host_request(header.message_id()) {
            if self.secondary_host_requests.is_empty() {
                tracing::warn!(id = ?header.message_id(), "received secondary host response with no pending request");
                return Ok(());
            }
            self.secondary_host_requests_read_send.send(buf.to_vec());
        } else {
            if self.primary_host_requests.is_empty() {
                tracing::warn!(id = ?header.message_id(), "received primary host response with no pending request");
                return Ok(());
            }
            self.read_send.send(buf.to_vec());
        }

        Ok(())
    }

    fn handle_update_generation_id(
        &mut self,
        response: get_protocol::UpdateGenerationId,
    ) -> Result<(), FatalError> {
        self.guest_notification_listeners
            .generation_id
            .send(response.generation_id)
            .map_err(|_| {
                FatalError::TooManyGuestNotifications(
                    get_protocol::GuestNotifications::UPDATE_GENERATION_ID,
                )
            })
    }

    fn handle_save_state_notification(
        &mut self,
        notification_header: get_protocol::SaveGuestVtl2StateNotification,
    ) -> Result<(), FatalError> {
        self.guest_notification_listeners
            .save_request
            .send(GuestSaveRequest {
                correlation_id: notification_header.correlation_id,
                timeout_hint: Duration::from_secs(notification_header.timeout_hint_secs.into()),
                capabilities_flags: notification_header.capabilities_flags,
            })
            .map_err(|_| {
                FatalError::TooManyGuestNotifications(
                    get_protocol::GuestNotifications::SAVE_GUEST_VTL2_STATE,
                )
            })
    }

    fn handle_modify_vtl2_settings_notification(&mut self, buf: &[u8]) -> Result<(), FatalError> {
        let (request, remaining) =
            get_protocol::ModifyVtl2SettingsNotification::read_from_prefix(buf).map_err(|_| {
                FatalError::MessageSizeGuestNotification {
                    len: buf.len(),
                    notification: get_protocol::GuestNotifications::MODIFY_VTL2_SETTINGS,
                }
            })?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

        let expected_len = request.size as usize;
        if remaining.len() != expected_len {
            return Err(FatalError::ModifyVtl2SettingsNotification {
                expected: expected_len,
                len: remaining.len(),
            });
        }

        self.send_vtl2_settings(
            remaining.into(),
            get_protocol::GuestNotifications::MODIFY_VTL2_SETTINGS,
        )
    }

    fn handle_modify_vtl2_settings_rev1_notification(
        &mut self,
        buf: &[u8],
    ) -> Result<(), FatalError> {
        let (request, remaining) =
            get_protocol::ModifyVtl2SettingsRev1Notification::read_from_prefix(buf).map_err(
                |_| FatalError::MessageSizeGuestNotification {
                    len: buf.len(),
                    notification: get_protocol::GuestNotifications::MODIFY_VTL2_SETTINGS_REV1,
                },
            )?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

        let expected_len = request.size as usize;
        if remaining.len() != expected_len {
            return Err(FatalError::ModifyVtl2SettingsNotification {
                expected: expected_len,
                len: remaining.len(),
            });
        }

        let vtl2_settings_buf = self.vtl2_settings_buf.take();
        let mut vtl2_settings_buf = vtl2_settings_buf.unwrap_or_default();
        vtl2_settings_buf.extend_from_slice(remaining);

        match request.payload_state {
            get_protocol::LargePayloadState::MORE => {
                self.vtl2_settings_buf = Some(vtl2_settings_buf);
                Ok(())
            }
            get_protocol::LargePayloadState::END => self.send_vtl2_settings(
                vtl2_settings_buf,
                get_protocol::GuestNotifications::MODIFY_VTL2_SETTINGS_REV1,
            ),
            _ => Err(FatalError::InvalidResponse),
        }
    }

    fn send_vtl2_settings(
        &mut self,
        vtl2_settings_buf: Vec<u8>,
        kind: get_protocol::GuestNotifications,
    ) -> Result<(), FatalError> {
        let res = self
            .guest_notification_listeners
            .vtl2_settings
            .try_call_failable(ModifyVtl2SettingsRequest, vtl2_settings_buf)
            .map_err(|_| FatalError::TooManyGuestNotifications(kind))?
            .map(GuestNotificationResponse::ModifyVtl2Settings)
            .boxed();

        self.guest_notification_responses.push(res);
        Ok(())
    }

    /// Receives VPCI device notifications and dispatches them to registered listeners.
    fn handle_vpci_device_notification(
        &mut self,
        notification: get_protocol::VpciDeviceNotification,
    ) -> Result<(), FatalError> {
        tracing::debug!(
            "Received VPCI device notification, bus id = {}, code = {:?}",
            notification.bus_instance_id,
            notification.code
        );

        if let Some(sender) = self
            .guest_notification_listeners
            .vpci
            .get(&notification.bus_instance_id)
        {
            let bus_event = match notification.code {
                get_protocol::VpciDeviceNotificationCode::ENUMERATED => {
                    VpciBusEvent::DeviceEnumerated
                }
                get_protocol::VpciDeviceNotificationCode::PREPARE_FOR_REMOVAL => {
                    VpciBusEvent::PrepareForRemoval
                }
                _ => return Err(FatalError::InvalidResponse),
            };
            sender.send(bus_event);
        }

        Ok(())
    }

    fn handle_battery_status_notification(
        &mut self,
        response: get_protocol::BatteryStatusNotification,
    ) -> Result<(), FatalError> {
        self.guest_notification_listeners
            .battery_status
            .send(HostBatteryUpdate {
                battery_present: response.flags.battery_present(),
                charging: response.flags.charging(),
                discharging: response.flags.discharging(),
                rate: response.rate,
                remaining_capacity: response.remaining_capacity,
                max_capacity: response.max_capacity,
                ac_online: response.flags.ac_online(),
            })
            .map_err(|_| {
                FatalError::TooManyGuestNotifications(
                    get_protocol::GuestNotifications::BATTERY_STATUS,
                )
            })
    }

    fn handle_debug_interrupt_notification(
        &mut self,
        notification: get_protocol::InjectDebugInterruptNotification,
    ) -> Result<(), FatalError> {
        tracing::debug!(
            "Received inject debug interrupt notification, vtl = {}",
            notification.vtl,
        );

        if notification.vtl != 0 {
            tracing::error!(
                ?notification.vtl,
                "Debug interrupts can only be injected into VTL 0",
            );
        } else {
            // Trigger the LINT1 interrupt vector on the LAPIC of the BSP.
            if let Some(callback) = self.set_debug_interrupt.as_ref() {
                callback(notification.vtl)
            }
        }

        Ok(())
    }

    fn handle_post_live_migration_notification(
        &mut self,
        _notification: get_protocol::PostLiveMigrationNotification,
    ) {
        tracing::info!(CVM_ALLOWED, "notify_post_live_migration");
        if let Some(callback) = self.post_live_migration.as_ref() {
            callback()
        }
    }

    fn complete_modify_vtl2_settings(
        &mut self,
        result: Result<(), RpcError<Vec<Vtl2SettingsErrorInfo>>>,
    ) -> Result<(), FatalError> {
        let errors = result.map_err(|err| match err {
            RpcError::Call(err) => err,
            RpcError::Channel(err) => vec![Vtl2SettingsErrorInfo::new(
                underhill_config::Vtl2SettingsErrorCode::InternalFailure,
                err.to_string(),
            )],
        });

        let (status, errors_json) = match errors {
            Ok(()) => (get_protocol::ModifyVtl2SettingsStatus::SUCCESS, None),
            Err(errors) => {
                let errors = Vtl2SettingsErrorInfoVec { errors };
                tracing::error!(
                    errors = &errors as &dyn std::error::Error,
                    "failed to modify vtl2 settings"
                );
                (
                    get_protocol::ModifyVtl2SettingsStatus::FAILURE,
                    Some(
                        serde_json::to_string(&errors.errors)
                            .map_err(FatalError::Vtl2SettingsErrorInfoJson)?,
                    ),
                )
            }
        };
        let errors_bytes = errors_json.as_ref().map(|json| json.as_bytes());
        let notification = get_protocol::ModifyVtl2SettingsCompleteNotification::new(
            status,
            errors_bytes.map_or(0, |v| v.len()) as u32,
        );
        let buf = [
            notification.as_bytes(),
            errors_bytes.unwrap_or(&[]).as_bytes(),
        ]
        .concat();
        self.send_message(buf);
        Ok(())
    }

    fn complete_start_vtl0(&mut self, error_msg: Option<String>) -> Result<(), FatalError> {
        let status = if error_msg.is_none() {
            get_protocol::StartVtl0Status::SUCCESS
        } else {
            get_protocol::StartVtl0Status::FAILURE
        };
        let error_bytes = error_msg.as_ref().map(|str| str.as_bytes());
        let notification = get_protocol::StartVtl0CompleteNotification::new(
            status,
            error_bytes.map_or(0, |v| v.len()) as u32,
        );
        let buf = [
            notification.as_bytes(),
            error_bytes.unwrap_or(&[]).as_bytes(),
        ]
        .concat();
        self.send_message(buf);
        Ok(())
    }

    fn report_restore_result_to_host(&mut self, success: bool) {
        let result = if success {
            get_protocol::GuestVtl2SaveRestoreStatus::SUCCESS
        } else {
            get_protocol::GuestVtl2SaveRestoreStatus::FAILURE
        };

        let host_notification = get_protocol::RestoreGuestVtl2StateHostNotification::new(result);
        self.send_message(host_notification.as_bytes().to_vec());
    }
}

async fn request_device_platform_settings_v2(
    mut access: HostRequestPipeAccess,
) -> Result<Vec<u8>, FatalError> {
    access.send_message(
        get_protocol::DevicePlatformSettingsRequestV2::new()
            .as_bytes()
            .to_vec(),
    );

    let mut result = Vec::new();
    loop {
        let buf = access.recv_response().await;
        let header = get_protocol::HeaderHostResponse::read_from_prefix(buf.as_slice())
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)

        // Protocol wart: request is sent as a
        // `HostRequests::DEVICE_PLATFORM_SETTINGS_V2`, but the host will send
        // back either `HostRequests::DEVICE_PLATFORM_SETTINGS_V2` *or* multiple
        // `HostRequests::DEVICE_PLATFORM_SETTINGS_V2_REV1` packets, depending
        // on how big the contained payload is.
        //
        // This is unfortunate, since not only is this a design principle
        // violation of how the GET is supposed to work, but it's also totally
        // useless, since `DEVICE_PLATFORM_SETTINGS_V2_REV1` is more than
        // capable of handling small payloads itself! We should've just "fixed"
        // `DEVICE_PLATFORM_SETTINGS_V2` before shipping... but we didn't, and
        // now we're stuck with this behavior.
        match header.message_id() {
            HostRequests::DEVICE_PLATFORM_SETTINGS_V2 => {
                let (response, remaining) =
                    get_protocol::DevicePlatformSettingsResponseV2::read_from_prefix(
                        buf.as_slice(),
                    )
                    .map_err(|_| FatalError::MessageSizeHostResponse {
                        len: buf.len(),
                        response: HostRequests::DEVICE_PLATFORM_SETTINGS_V2,
                    })?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

                if response.size as usize != remaining.len() {
                    return Err(FatalError::DevicePlatformSettingsV2Payload {
                        expected: response.size as usize,
                        len: remaining.len(),
                    });
                }

                result.extend(remaining);
                break;
            }
            HostRequests::DEVICE_PLATFORM_SETTINGS_V2_REV1 => {
                let (response, remaining) =
                    get_protocol::DevicePlatformSettingsResponseV2Rev1::read_from_prefix(
                        buf.as_slice(),
                    )
                    .map_err(|_| FatalError::MessageSizeGuestNotification {
                        len: buf.len(),
                        notification: get_protocol::GuestNotifications::MODIFY_VTL2_SETTINGS_REV1,
                    })?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

                if remaining.len() != (response.size as usize) {
                    return Err(FatalError::DevicePlatformSettingsV2Payload {
                        expected: response.size as usize,
                        len: remaining.len(),
                    });
                }

                result.extend(remaining);
                if response.payload_state == get_protocol::LargePayloadState::END {
                    break;
                }
            }
            _ => {
                return Err(FatalError::ResponseHeaderMismatchId(
                    header.message_id(),
                    HostRequests::DEVICE_PLATFORM_SETTINGS_V2,
                ));
            }
        }
    }
    Ok(result)
}

async fn request_vmgs_read(
    mut access: HostRequestPipeAccess,
    input: msg::VmgsReadInput,
) -> Result<Result<Vec<u8>, get_protocol::VmgsReadResponse>, FatalError> {
    let msg::VmgsReadInput {
        sector_offset,
        sector_count,
        sector_size,
    } = input;
    access.send_message(
        get_protocol::VmgsReadRequest::new(
            get_protocol::VmgsReadFlags::NONE,
            sector_offset,
            sector_count,
        )
        .as_bytes()
        .to_vec(),
    );

    let buf = access.recv_response().await;

    let vmgs_buf_len = (sector_count * sector_size) as usize;
    let (response, remaining) = get_protocol::VmgsReadResponse::read_from_prefix(buf.as_slice())
        .map_err(|_| FatalError::MessageSizeHostResponse {
            len: buf.len(),
            response: HostRequests::VMGS_READ,
        })?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

    if response.message_header.message_id() != HostRequests::VMGS_READ {
        return Err(FatalError::ResponseHeaderMismatchId(
            response.message_header.message_id(),
            HostRequests::VMGS_READ,
        ));
    }

    if response.status != get_protocol::VmgsIoStatus::SUCCESS {
        return Ok(Err(response));
    }

    // get size of buffer and read in buffer
    if remaining.len() != vmgs_buf_len {
        return Err(FatalError::MessageSizeHostResponse {
            len: buf.len(),
            response: HostRequests::VMGS_READ,
        });
    }

    Ok(Ok(remaining.to_vec()))
}

async fn request_vmgs_write(
    mut access: HostRequestPipeAccess,
    input: msg::VmgsWriteInput,
) -> Result<Result<(), get_protocol::VmgsWriteResponse>, FatalError> {
    let request = get_protocol::VmgsWriteRequest::new(
        get_protocol::VmgsWriteFlags::NONE,
        input.sector_offset,
        (input.buf.len() / input.sector_size as usize) as u32,
    );
    let message = [request.as_bytes(), &input.buf].concat();
    let response: get_protocol::VmgsWriteResponse =
        access.send_request_fixed_size(message.as_slice()).await?;
    if response.status != get_protocol::VmgsIoStatus::SUCCESS {
        return Ok(Err(response));
    }
    Ok(Ok(()))
}

async fn request_send_servicing_state(
    mut access: HostRequestPipeAccess,
    result: Result<Vec<u8>, String>,
) -> Result<Result<(), ()>, FatalError> {
    let saved_state_buf = match result {
        Ok(saved_state_buf) => saved_state_buf,
        Err(_err) => {
            // Sends a failure notification to host.
            return access
                .send_failed_save_state(&get_protocol::SaveGuestVtl2StateRequest::new(
                    get_protocol::GuestVtl2SaveRestoreStatus::FAILURE,
                ))
                .await
                .map(Ok);
        }
    };

    let mut saved_state_bytes_written = 0;

    let saved_state_size = saved_state_buf.len();
    const HEADER_SIZE: usize = size_of::<get_protocol::SaveGuestVtl2StateRequest>();

    while saved_state_bytes_written < saved_state_size {
        let status_code = if saved_state_bytes_written + MAX_PAYLOAD_SIZE >= saved_state_size {
            get_protocol::GuestVtl2SaveRestoreStatus::SUCCESS
        } else {
            get_protocol::GuestVtl2SaveRestoreStatus::MORE_DATA
        };

        let host_request_header = get_protocol::SaveGuestVtl2StateRequest::new(status_code);
        let payload_len = min(
            saved_state_size - saved_state_bytes_written,
            MAX_PAYLOAD_SIZE,
        );

        tracing::debug!(
            "More data? {:?} saved_state_bytes_written {} saved_state_size {}, payload_len {}",
            status_code,
            saved_state_bytes_written,
            saved_state_size,
            payload_len
        );

        let mut message = vec![0; HEADER_SIZE + payload_len];

        message[..HEADER_SIZE].copy_from_slice(host_request_header.as_bytes());
        message[HEADER_SIZE..].copy_from_slice(
            saved_state_buf[saved_state_bytes_written..][..payload_len].as_bytes(),
        );

        access.send_message(message);
        saved_state_bytes_written += payload_len;
    }

    tracing::debug!("Done writing saved state, awaiting host response");

    let response: get_protocol::SaveGuestVtl2StateResponse = access
        .recv_response_fixed_size(HostRequests::SAVE_GUEST_VTL2_STATE)
        .await?;

    match response.save_status {
        get_protocol::GuestVtl2SaveRestoreStatus::SUCCESS => Ok(Ok(())),
        get_protocol::GuestVtl2SaveRestoreStatus::FAILURE => Ok(Err(())),
        _ => Err(FatalError::InvalidResponse),
    }
}

async fn request_saved_state(
    mut access: HostRequestPipeAccess,
) -> Result<Result<Vec<u8>, ()>, FatalError> {
    access.send_message(
        get_protocol::RestoreGuestVtl2StateRequest::new(
            get_protocol::GuestVtl2SaveRestoreStatus::REQUEST_DATA,
        )
        .as_bytes()
        .to_vec(),
    );

    let mut saved_state_buf = Vec::<u8>::new();

    loop {
        let message_buf = access.recv_response().await;

        let (response_header, remaining) =
            get_protocol::RestoreGuestVtl2StateResponse::read_from_prefix(message_buf.as_slice())
                .map_err(|_| FatalError::MessageSizeHostResponse {
                len: message_buf.len(),
                response: HostRequests::RESTORE_GUEST_VTL2_STATE,
            })?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

        let message_id = response_header.message_header.message_id();
        if message_id != HostRequests::RESTORE_GUEST_VTL2_STATE {
            return Err(FatalError::ResponseHeaderMismatchId(
                message_id,
                HostRequests::RESTORE_GUEST_VTL2_STATE,
            ));
        }

        if response_header.data_length as usize != remaining.len() {
            return Err(FatalError::InvalidResponse);
        }

        match response_header.restore_status {
            get_protocol::GuestVtl2SaveRestoreStatus::SUCCESS => {
                saved_state_buf.extend_from_slice(remaining);
                break;
            }
            get_protocol::GuestVtl2SaveRestoreStatus::MORE_DATA => {
                saved_state_buf.extend_from_slice(remaining);
            }
            get_protocol::GuestVtl2SaveRestoreStatus::FAILURE => {
                return Ok(Err(()));
            }
            _ => return Err(FatalError::InvalidResponse),
        }
    }

    Ok(Ok(saved_state_buf))
}

/// Send a VPCI device control request to the host and wait for the result asynchronously.
async fn request_vpci_device_control(
    mut access: HostRequestPipeAccess,
    input: msg::VpciDeviceControlInput,
) -> Result<get_protocol::VpciDeviceControlResponse, FatalError> {
    let response: get_protocol::VpciDeviceControlResponse = access
        .send_request_fixed_size(&get_protocol::VpciDeviceControlRequest::new(
            input.code.0,
            input.bus_instance_id,
        ))
        .await?;

    Ok(response)
}

/// Send the attestation request to the IGVm agent on the host and wait for the result asynchronously.
///
/// Because the response size can be too large to fit vmbus,
/// the function sets up shared memory (up to `IGVM_ATTEST_MSG_MAX_SHARED_GPA`)
/// and passes the list of GPAs to GED, which will then write the response
/// payload to the shared memory.
///
/// Returns `Ok(Ok(Vec<u8>)` if the request succeeds.
/// Returns `Ok(Err(IgvmAttestError))` if an non-fatal error occurs. The error will be propagated
/// to the client.
/// Returns Err(FatalError) otherwise.
async fn request_igvm_attest(
    mut access: HostRequestPipeAccess,
    request: msg::IgvmAttestRequestData,
    gpa_allocator: Option<Arc<dyn DmaClient>>,
) -> Result<Result<Vec<u8>, IgvmAttestError>, FatalError> {
    let allocator = gpa_allocator.ok_or(FatalError::GpaAllocatorUnavailable)?;
    let dma_size = request.response_buffer_len;
    let mem = allocator
        .allocate_dma_buffer(dma_size)
        .map_err(FatalError::GpaMemoryAllocationError)?;

    // Host expects the vTOM bit to be stripped
    let pfn_bias = mem.pfn_bias();
    let gpas = mem
        .pfns()
        .iter()
        .map(|pfn| (pfn & !(pfn_bias)) * hvdef::HV_PAGE_SIZE)
        .collect::<Vec<_>>();

    let mut shared_gpa = [0u64; get_protocol::IGVM_ATTEST_MSG_MAX_SHARED_GPA];
    shared_gpa[..gpas.len()].copy_from_slice(&gpas);

    let request =
        match prepare_igvm_attest_request(shared_gpa, &request.agent_data, &request.report) {
            Ok(request) => request,
            Err(e) => return Ok(Err(e)),
        };

    access.send_message(request.as_bytes().to_vec());

    let response = access.recv_response().await;

    // Validate the response and returns the validated data.
    // TODO: zerocopy: use error here, use rest of range (https://github.com/microsoft/openvmm/issues/759)
    let Ok((response, _)) = get_protocol::IgvmAttestResponse::read_from_prefix(&response) else {
        Err(FatalError::DeserializeIgvmAttestResponse)?
    };

    let response_length = response.length as usize;
    if response_length == get_protocol::IGVM_ATTEST_VMWP_GENERIC_ERROR_CODE {
        return Ok(Err(IgvmAttestError::IgvmAgentGenericError));
    } else if response_length > dma_size {
        Err(FatalError::InvalidIgvmAttestResponseSize {
            response_size: response_length,
            maximum_size: dma_size,
        })?
    }

    let mut buffer = vec![0u8; dma_size];
    mem.read_at(0, &mut buffer);

    buffer.truncate(response_length);

    Ok(Ok(buffer))
}

/// Prepare the `IgvmAttest` request.
fn prepare_igvm_attest_request(
    shared_gpa: [u64; get_protocol::IGVM_ATTEST_MSG_MAX_SHARED_GPA],
    agent_data: &[u8],
    report: &[u8],
) -> Result<get_protocol::IgvmAttestRequest, IgvmAttestError> {
    use get_protocol::IGVM_ATTEST_MSG_REQ_AGENT_DATA_MAX_SIZE;
    use get_protocol::IGVM_ATTEST_MSG_REQ_REPORT_MAX_SIZE;

    if agent_data.len() > IGVM_ATTEST_MSG_REQ_AGENT_DATA_MAX_SIZE {
        Err(IgvmAttestError::InvalidAgentDataSize {
            input_size: agent_data.len(),
            expected_size: IGVM_ATTEST_MSG_REQ_AGENT_DATA_MAX_SIZE,
        })?
    }
    if report.len() > IGVM_ATTEST_MSG_REQ_REPORT_MAX_SIZE {
        Err(IgvmAttestError::InvalidReportSize {
            input_size: report.len(),
            expected_size: IGVM_ATTEST_MSG_REQ_REPORT_MAX_SIZE,
        })?
    }

    let mut agent_data_max = [0u8; IGVM_ATTEST_MSG_REQ_AGENT_DATA_MAX_SIZE];
    agent_data_max[..agent_data.len()].copy_from_slice(agent_data);

    let mut report_max = [0u8; IGVM_ATTEST_MSG_REQ_REPORT_MAX_SIZE];
    report_max[..report.len()].copy_from_slice(report);

    Ok(get_protocol::IgvmAttestRequest::new(
        shared_gpa,
        shared_gpa.len() as u32,
        agent_data_max,
        agent_data.len() as u32,
        report_max,
        report.len() as u32,
    ))
}
