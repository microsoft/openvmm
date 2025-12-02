// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub mod saved_state;
#[cfg(test)]
mod tests;

use crate::Guid;
use crate::SynicMessage;
use crate::monitor::AssignedMonitors;
use crate::protocol::Version;
use hvdef::Vtl;
use inspect::Inspect;
pub use saved_state::RestoreError;
pub use saved_state::SavedState;
pub use saved_state::SavedStateData;
use slab::Slab;
use std::cmp::min;
use std::collections::VecDeque;
use std::collections::hash_map::Entry;
use std::collections::hash_map::HashMap;
use std::fmt::Display;
use std::ops::Index;
use std::ops::IndexMut;
use std::task::Poll;
use std::task::ready;
use std::time::Duration;
use thiserror::Error;
use vmbus_channel::bus::ChannelType;
use vmbus_channel::bus::GpadlRequest;
use vmbus_channel::bus::OfferKey;
use vmbus_channel::bus::OfferParams;
use vmbus_channel::bus::OpenData;
use vmbus_channel::bus::RestoredGpadl;
use vmbus_core::HvsockConnectRequest;
use vmbus_core::HvsockConnectResult;
use vmbus_core::MaxVersionInfo;
use vmbus_core::OutgoingMessage;
use vmbus_core::VMBUS_SINT;
use vmbus_core::VersionInfo;
use vmbus_core::protocol;
use vmbus_core::protocol::ChannelId;
use vmbus_core::protocol::ConnectionId;
use vmbus_core::protocol::FeatureFlags;
use vmbus_core::protocol::GpadlId;
use vmbus_core::protocol::Message;
use vmbus_core::protocol::OfferFlags;
use vmbus_core::protocol::UserDefinedData;
use vmbus_ring::gparange;
use vmcore::monitor::MonitorId;
use vmcore::synic::MonitorInfo;
use vmcore::synic::MonitorPageGpas;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// An error caused by a channel operation.
#[derive(Debug, Error)]
pub enum ChannelError {
    #[error("unknown channel ID")]
    UnknownChannelId,
    #[error("unknown GPADL ID")]
    UnknownGpadlId,
    #[error("parse error")]
    ParseError(#[from] protocol::ParseError),
    #[error("invalid gpa range")]
    InvalidGpaRange(#[source] gparange::Error),
    #[error("duplicate GPADL ID")]
    DuplicateGpadlId,
    #[error("GPADL is already complete")]
    GpadlAlreadyComplete,
    #[error("GPADL channel ID mismatch")]
    WrongGpadlChannelId,
    #[error("trying to open an open channel")]
    ChannelAlreadyOpen,
    #[error("trying to close a closed channel")]
    ChannelNotOpen,
    #[error("invalid GPADL state for operation")]
    InvalidGpadlState,
    #[error("invalid channel state for operation")]
    InvalidChannelState,
    #[error("channel ID has already been released")]
    ChannelReleased,
    #[error("channel offers have already been sent")]
    OffersAlreadySent,
    #[error("invalid operation on reserved channel")]
    ChannelReserved,
    #[error("invalid operation on non-reserved channel")]
    ChannelNotReserved,
    #[error("received untrusted message for trusted connection")]
    UntrustedMessage,
    #[error("received a non-resuming message while paused")]
    Paused,
}

#[derive(Debug, Error)]
pub enum OfferError {
    #[error("the channel ID {} is not valid for this operation", (.0).0)]
    InvalidChannelId(ChannelId),
    #[error("the channel ID {} is already in use", (.0).0)]
    ChannelIdInUse(ChannelId),
    #[error("offer {0} already exists")]
    AlreadyExists(OfferKey),
    #[error("specified resources do not match those of the existing saved or revoked offer")]
    IncompatibleResources,
    #[error("too many channels have been offered")]
    TooManyChannels,
    #[error("mismatched monitor ID from saved state; expected {0:?}, actual {1:?}")]
    MismatchedMonitorId(Option<MonitorId>, MonitorId),
}

/// A unique identifier for an offered channel.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OfferId(usize);

type IncompleteGpadlMap = HashMap<GpadlId, OfferId>;

type GpadlMap = HashMap<(GpadlId, OfferId), Gpadl>;

/// A struct modeling the server side of the VMBus control plane.
pub struct Server {
    state: ConnectionState,
    channels: ChannelList,
    assigned_channels: AssignedChannels,
    assigned_monitors: AssignedMonitors,
    gpadls: GpadlMap,
    incomplete_gpadls: IncompleteGpadlMap,
    child_connection_id: u32,
    max_version: Option<MaxVersionInfo>,
    delayed_max_version: Option<MaxVersionInfo>,
    // This must be separate from the connection state because e.g. the UnloadComplete message,
    // or messages for reserved channels, can be pending even when disconnected.
    pending_messages: PendingMessages,
    // If this is set, the server cannot utilize monitor pages provided by the guest. This is
    // typically the case for OpenHCL in hardware-isolated VMs because the monitor pages must be in
    // shared memory and we cannot set protections on shared memory.
    require_server_allocated_mnf: bool,
    use_absolute_channel_order: bool,
}

pub struct ServerWithNotifier<'a, T> {
    inner: &'a mut Server,
    notifier: &'a mut T,
}

impl<T> Drop for ServerWithNotifier<'_, T> {
    fn drop(&mut self) {
        self.inner.validate();
    }
}

impl<T: Notifier> Inspect for ServerWithNotifier<'_, T> {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        let (state, info, next_action) = match &self.inner.state {
            ConnectionState::Disconnected => ("disconnected", None, None),
            ConnectionState::Connecting { info, .. } => ("connecting", Some(info), None),
            ConnectionState::Connected(info) => (
                if info.offers_sent {
                    "connected"
                } else {
                    "negotiated"
                },
                Some(info),
                None,
            ),
            ConnectionState::Disconnecting { next_action, .. } => {
                ("disconnecting", None, Some(next_action))
            }
        };

        resp.field("connection_info", info);
        let next_action = next_action.map(|a| match a {
            ConnectionAction::None => "disconnect",
            ConnectionAction::Reset => "reset",
            ConnectionAction::SendUnloadComplete => "unload",
            ConnectionAction::Reconnect { .. } => "reconnect",
            ConnectionAction::SendFailedVersionResponse => "send_version_response",
        });
        resp.field("state", state)
            .field("next_action", next_action)
            .field(
                "assigned_monitors_bitmap",
                format_args!("{:x}", self.inner.assigned_monitors.bitmap()),
            )
            .child("channels", |req| {
                let mut resp = req.respond();
                self.inner
                    .channels
                    .inspect(self.notifier, self.inner.get_version(), &mut resp);
                for ((gpadl_id, offer_id), gpadl) in &self.inner.gpadls {
                    let channel = &self.inner.channels[*offer_id];
                    resp.field(
                        &channel_inspect_path(
                            &channel.offer,
                            format_args!("/gpadls/{}", gpadl_id.0),
                        ),
                        gpadl,
                    );
                }
            });
    }
}

/// Stores the monitor page GPAs along with their source.
#[derive(Debug, Copy, Clone, Inspect)]
struct MonitorPageGpaInfo {
    gpas: MonitorPageGpas,
    server_allocated: bool,
}

impl MonitorPageGpaInfo {
    /// Creates a new MonitorPageGpaInfo from guest-provided GPAs.
    fn from_guest_gpas(gpas: MonitorPageGpas) -> Self {
        Self {
            gpas,
            server_allocated: false,
        }
    }

    /// Creates a new MonitorPageGpaInfo from server-allocated GPAs.
    fn from_server_gpas(gpas: MonitorPageGpas) -> Self {
        Self {
            gpas,
            server_allocated: true,
        }
    }
}

#[derive(Debug, Copy, Clone, Inspect)]
struct ConnectionInfo {
    version: VersionInfo,
    // Indicates if the connection is trusted for the paravisor of a hardware-isolated VM. In other
    // cases, this value is always false.
    trusted: bool,
    offers_sent: bool,
    interrupt_page: Option<u64>,
    monitor_page: Option<MonitorPageGpaInfo>,
    target_message_vp: u32,
    modifying: bool,
    client_id: Guid,
    paused: bool,
}

/// The state of the VMBus connection.
#[derive(Debug)]
enum ConnectionState {
    Disconnected,
    Disconnecting {
        next_action: ConnectionAction,
        modify_sent: bool,
    },
    Connecting {
        info: ConnectionInfo,
        next_action: ConnectionAction,
    },
    Connected(ConnectionInfo),
}

impl ConnectionState {
    /// Checks whether the state is connected using at least the specified version.
    fn check_version(&self, min_version: Version) -> bool {
        matches!(self, ConnectionState::Connected(info) if info.version.version >= min_version)
    }

    /// Checks whether the state is connected and the specified predicate holds for the feature
    /// flags.
    fn check_feature_flags(&self, flags: impl Fn(FeatureFlags) -> bool) -> bool {
        matches!(self, ConnectionState::Connected(info) if flags(info.version.feature_flags))
    }

    fn get_version(&self) -> Option<VersionInfo> {
        if let ConnectionState::Connected(info) = self {
            Some(info.version)
        } else {
            None
        }
    }

    /// Gets the `ConnectionInfo` if currently connected.
    fn get_connected_info(&self) -> Option<&ConnectionInfo> {
        if let ConnectionState::Connected(info) = self {
            Some(info)
        } else {
            None
        }
    }

    fn is_trusted(&self) -> bool {
        match self {
            ConnectionState::Connected(info) => info.trusted,
            ConnectionState::Connecting { info, .. } => info.trusted,
            _ => false,
        }
    }

    fn is_paused(&self) -> bool {
        if let ConnectionState::Connected(info) = self {
            info.paused
        } else {
            false
        }
    }
}

#[derive(Debug, Copy, Clone)]
enum ConnectionAction {
    None,
    Reset,
    SendUnloadComplete,
    Reconnect {
        initiate_contact: InitiateContactRequest,
    },
    SendFailedVersionResponse,
}

#[derive(PartialEq, Eq, Debug, Copy, Clone)]
pub enum MonitorPageRequest {
    None,
    Some(MonitorPageGpas),
    Invalid,
}

#[derive(PartialEq, Eq, Debug, Copy, Clone)]
pub struct InitiateContactRequest {
    pub version_requested: u32,
    pub target_message_vp: u32,
    pub monitor_page: MonitorPageRequest,
    pub target_sint: u8,
    pub target_vtl: u8,
    pub feature_flags: u32,
    pub interrupt_page: Option<u64>,
    pub client_id: Guid,
    pub trusted: bool,
}

#[derive(Debug, Copy, Clone)]
pub struct OpenRequest {
    pub open_id: u32,
    pub ring_buffer_gpadl_id: GpadlId,
    pub target_vp: u32,
    pub downstream_ring_buffer_page_offset: u32,
    pub user_data: UserDefinedData,
    pub guest_specified_interrupt_info: Option<SignalInfo>,
    pub flags: protocol::OpenChannelFlags,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum Update<T: std::fmt::Debug + Copy + Clone> {
    Unchanged,
    Reset,
    Set(T),
}

impl<T: std::fmt::Debug + Copy + Clone> From<Option<T>> for Update<T> {
    fn from(value: Option<T>) -> Self {
        match value {
            None => Self::Reset,
            Some(value) => Self::Set(value),
        }
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct ModifyConnectionRequest {
    pub version: Option<VersionInfo>,
    pub monitor_page: Update<MonitorPageGpas>,
    pub interrupt_page: Update<u64>,
    pub target_message_vp: Option<u32>,
    pub notify_relay: bool,
}

// Manual implementation because notify_relay should be true by default.
impl Default for ModifyConnectionRequest {
    fn default() -> Self {
        Self {
            version: None,
            monitor_page: Update::Unchanged,
            interrupt_page: Update::Unchanged,
            target_message_vp: None,
            notify_relay: true,
        }
    }
}

impl From<protocol::ModifyConnection> for ModifyConnectionRequest {
    fn from(value: protocol::ModifyConnection) -> Self {
        let monitor_page = if value.parent_to_child_monitor_page_gpa != 0 {
            Update::Set(MonitorPageGpas {
                parent_to_child: value.parent_to_child_monitor_page_gpa,
                child_to_parent: value.child_to_parent_monitor_page_gpa,
            })
        } else {
            Update::Reset
        };

        Self {
            monitor_page,
            ..Default::default()
        }
    }
}

/// Response to a ModifyConnectionRequest.
#[derive(Debug, Copy, Clone)]
pub enum ModifyConnectionResponse {
    /// The requested version change is supported, and the relay completed the connection
    /// modification with the specified status and supports the specified feature flags. All of the
    /// feature flags supported by the relay host are included, regardless of what features were
    /// requested. If the server allocated monitor pages that are to be used for this connection,
    /// they will be included as well.
    Supported(
        protocol::ConnectionState,
        FeatureFlags,
        Option<MonitorPageGpas>,
    ),
    /// A version change was requested but the relay host doesn't support that version.
    Unsupported,
    /// The connection modification completed with the specified status. This response type must be
    /// sent if and only if no version change was requested.
    Modified(protocol::ConnectionState),
}

#[derive(Debug, Copy, Clone)]
pub enum ModifyState {
    NotModifying,
    Modifying { pending_target_vp: Option<u32> },
}

impl ModifyState {
    pub fn is_modifying(&self) -> bool {
        matches!(self, ModifyState::Modifying { .. })
    }
}

#[derive(Debug, Copy, Clone)]
pub struct SignalInfo {
    pub event_flag: u16,
    pub connection_id: u32,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum RestoreState {
    /// The channel has been offered newly this session.
    New,
    /// The channel was in the saved state and has been re-offered this session,
    /// but restore_channel has not yet been called on it, and revoke_unclaimed_channels
    /// has not yet been called.
    Restoring,
    /// The channel was in the saved state but has not yet been re-offered this
    /// session.
    Unmatched,
    /// The channel was in the saved state and is now in a fully restored state.
    Restored,
}

/// The state of a single vmbus channel.
#[derive(Debug, Clone)]
enum ChannelState {
    /// The device has offered the channel but the offer has not been sent to the
    /// guest. However, there may still be GPADLs for this channel from a
    /// previous connection.
    ClientReleased,

    /// The channel has been offered to the guest.
    Closed,

    /// The guest has requested to open the channel and the device has been
    /// notified.
    Opening {
        request: OpenRequest,
        reserved_state: Option<ReservedState>,
    },

    /// The channel is open by both the guest and the device.
    Open {
        params: OpenRequest,
        modify_state: ModifyState,
        reserved_state: Option<ReservedState>,
    },

    /// The device has been notified to close the channel.
    Closing {
        params: OpenRequest,
        reserved_state: Option<ReservedState>,
    },

    /// The device has been notified to close the channel, and the guest has
    /// requested to reopen it.
    ClosingReopen {
        params: OpenRequest,
        request: OpenRequest,
    },

    /// The device has revoked the channel but the guest has not released it yet.
    Revoked,

    /// The device has been reoffered, but the guest has not released the previous
    /// offer yet.
    Reoffered,

    /// The guest has released the channel but there is still a pending close
    /// request to the device.
    ClosingClientRelease,

    /// The guest has released the channel, but there is still a pending open
    /// request to the device.
    OpeningClientRelease,
}

impl ChannelState {
    /// If true, the channel is unreferenced by the guest, and the guest should
    /// not be able to perform operations on the channel.
    fn is_released(&self) -> bool {
        match self {
            ChannelState::Closed
            | ChannelState::Opening { .. }
            | ChannelState::Open { .. }
            | ChannelState::Closing { .. }
            | ChannelState::ClosingReopen { .. }
            | ChannelState::Revoked
            | ChannelState::Reoffered => false,

            ChannelState::ClientReleased
            | ChannelState::ClosingClientRelease
            | ChannelState::OpeningClientRelease => true,
        }
    }

    /// If true, the channel has been revoked.
    fn is_revoked(&self) -> bool {
        match self {
            ChannelState::Revoked | ChannelState::Reoffered => true,

            ChannelState::ClientReleased
            | ChannelState::Closed
            | ChannelState::Opening { .. }
            | ChannelState::Open { .. }
            | ChannelState::Closing { .. }
            | ChannelState::ClosingReopen { .. }
            | ChannelState::ClosingClientRelease
            | ChannelState::OpeningClientRelease => false,
        }
    }

    fn is_reserved(&self) -> bool {
        match self {
            // TODO: Should closing be included here?
            ChannelState::Open {
                reserved_state: Some(_),
                ..
            }
            | ChannelState::Opening {
                reserved_state: Some(_),
                ..
            }
            | ChannelState::Closing {
                reserved_state: Some(_),
                ..
            } => true,

            ChannelState::Opening { .. }
            | ChannelState::Open { .. }
            | ChannelState::Closing { .. }
            | ChannelState::ClientReleased
            | ChannelState::Closed
            | ChannelState::ClosingReopen { .. }
            | ChannelState::Revoked
            | ChannelState::Reoffered
            | ChannelState::ClosingClientRelease
            | ChannelState::OpeningClientRelease => false,
        }
    }
}

impl Display for ChannelState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match self {
            Self::ClientReleased => "ClientReleased",
            Self::Closed => "Closed",
            Self::Opening { .. } => "Opening",
            Self::Open { .. } => "Open",
            Self::Closing { .. } => "Closing",
            Self::ClosingReopen { .. } => "ClosingReopen",
            Self::Revoked => "Revoked",
            Self::Reoffered => "Reoffered",
            Self::ClosingClientRelease => "ClosingClientRelease",
            Self::OpeningClientRelease => "OpeningClientRelease",
        };
        write!(f, "{}", state)
    }
}

/// Indicates how a MNF (monitored interrupts) should be used for a channel.
#[derive(Debug, Clone, Default, mesh::MeshPayload)]
pub enum MnfUsage {
    /// The channel does not use MNF.
    #[default]
    Disabled,
    /// The channel uses MNF, handled by this server, with the specified interrupt latency.
    Enabled { latency: Duration },
    /// The channel uses MNF, handled by the relay host, with the monitor ID specified by the relay
    /// host.
    Relayed { monitor_id: u8 },
}

impl MnfUsage {
    pub fn is_enabled(&self) -> bool {
        matches!(self, Self::Enabled { .. })
    }

    pub fn is_relayed(&self) -> bool {
        matches!(self, Self::Relayed { .. })
    }

    pub fn enabled_and_then<T>(&self, f: impl FnOnce(Duration) -> Option<T>) -> Option<T> {
        if let Self::Enabled { latency } = self {
            f(*latency)
        } else {
            None
        }
    }
}

impl From<Option<Duration>> for MnfUsage {
    fn from(value: Option<Duration>) -> Self {
        match value {
            None => Self::Disabled,
            Some(latency) => Self::Enabled { latency },
        }
    }
}

#[derive(Debug, Clone, Default, mesh::MeshPayload)]
pub struct OfferParamsInternal {
    /// An informational string describing the channel type.
    pub interface_name: String,
    pub instance_id: Guid,
    pub interface_id: Guid,
    pub mmio_megabytes: u16,
    pub mmio_megabytes_optional: u16,
    pub subchannel_index: u16,
    pub use_mnf: MnfUsage,
    pub offer_order: Option<u64>,
    pub flags: OfferFlags,
    pub user_defined: UserDefinedData,
}

impl OfferParamsInternal {
    /// Gets the offer key for this offer.
    pub fn key(&self) -> OfferKey {
        OfferKey {
            interface_id: self.interface_id,
            instance_id: self.instance_id,
            subchannel_index: self.subchannel_index,
        }
    }
}

impl From<OfferParams> for OfferParamsInternal {
    fn from(value: OfferParams) -> Self {
        let mut user_defined = UserDefinedData::new_zeroed();

        // All non-relay channels are capable of using a confidential ring buffer, but external
        // memory is dependent on the device.
        let mut flags = OfferFlags::new()
            .with_confidential_ring_buffer(true)
            .with_confidential_external_memory(value.allow_confidential_external_memory);

        match value.channel_type {
            ChannelType::Device { pipe_packets } => {
                if pipe_packets {
                    flags.set_named_pipe_mode(true);
                    user_defined.as_pipe_params_mut().pipe_type = protocol::PipeType::MESSAGE;
                }
            }
            ChannelType::Interface {
                user_defined: interface_user_defined,
            } => {
                flags.set_enumerate_device_interface(true);
                user_defined = interface_user_defined;
            }
            ChannelType::Pipe { message_mode } => {
                flags.set_enumerate_device_interface(true);
                flags.set_named_pipe_mode(true);
                user_defined.as_pipe_params_mut().pipe_type = if message_mode {
                    protocol::PipeType::MESSAGE
                } else {
                    protocol::PipeType::BYTE
                };
            }
            ChannelType::HvSocket {
                is_connect,
                is_for_container,
                silo_id,
            } => {
                flags.set_enumerate_device_interface(true);
                flags.set_tlnpi_provider(true);
                flags.set_named_pipe_mode(true);
                *user_defined.as_hvsock_params_mut() = protocol::HvsockUserDefinedParameters::new(
                    is_connect,
                    is_for_container,
                    silo_id,
                );
            }
        };

        Self {
            interface_name: value.interface_name,
            instance_id: value.instance_id,
            interface_id: value.interface_id,
            mmio_megabytes: value.mmio_megabytes,
            mmio_megabytes_optional: value.mmio_megabytes_optional,
            subchannel_index: value.subchannel_index,
            use_mnf: value.mnf_interrupt_latency.into(),
            offer_order: value.offer_order,
            user_defined,
            flags,
        }
    }
}

#[derive(Debug, Copy, Clone, Inspect, PartialEq, Eq)]
pub struct ConnectionTarget {
    pub vp: u32,
    pub sint: u8,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum MessageTarget {
    Default,
    ReservedChannel(OfferId, ConnectionTarget),
    Custom(ConnectionTarget),
}

impl MessageTarget {
    pub fn for_offer(offer_id: OfferId, reserved_state: &Option<ReservedState>) -> Self {
        if let Some(state) = reserved_state {
            Self::ReservedChannel(offer_id, state.target)
        } else {
            Self::Default
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub struct ReservedState {
    version: VersionInfo,
    target: ConnectionTarget,
}

/// A VMBus channel.
#[derive(Debug)]
struct Channel {
    info: Option<OfferedInfo>,
    offer: OfferParamsInternal,
    state: ChannelState,
    restore_state: RestoreState,
}

#[derive(Debug, Copy, Clone)]
struct OfferedInfo {
    channel_id: ChannelId,
    connection_id: u32,
    monitor_id: Option<MonitorId>,
}

impl Channel {
    fn inspect_state(&self, resp: &mut inspect::Response<'_>) {
        let mut target_vp = None;
        let mut event_flag = None;
        let mut connection_id = None;
        let mut reserved_target = None;
        let state = match &self.state {
            ChannelState::ClientReleased => "client_released",
            ChannelState::Closed => "closed",
            ChannelState::Opening { reserved_state, .. } => {
                reserved_target = reserved_state.map(|state| state.target);
                "opening"
            }
            ChannelState::Open {
                params,
                reserved_state,
                ..
            } => {
                target_vp = Some(params.target_vp);
                if let Some(id) = params.guest_specified_interrupt_info {
                    event_flag = Some(id.event_flag);
                    connection_id = Some(id.connection_id);
                }
                reserved_target = reserved_state.map(|state| state.target);
                "open"
            }
            ChannelState::Closing { reserved_state, .. } => {
                reserved_target = reserved_state.map(|state| state.target);
                "closing"
            }
            ChannelState::ClosingReopen { .. } => "closing_reopen",
            ChannelState::Revoked => "revoked",
            ChannelState::Reoffered => "reoffered",
            ChannelState::ClosingClientRelease => "closing_client_release",
            ChannelState::OpeningClientRelease => "opening_client_release",
        };
        let restore_state = match self.restore_state {
            RestoreState::New => "new",
            RestoreState::Restoring => "restoring",
            RestoreState::Restored => "restored",
            RestoreState::Unmatched => "unmatched",
        };
        if let Some(info) = &self.info {
            resp.field("channel_id", info.channel_id.0)
                .field("offered_connection_id", info.connection_id)
                .field("monitor_id", info.monitor_id.map(|id| id.0));
        }
        resp.field("state", state)
            .field("restore_state", restore_state)
            .field("interface_name", self.offer.interface_name.clone())
            .display("instance_id", &self.offer.instance_id)
            .display("interface_id", &self.offer.interface_id)
            .field("mmio_megabytes", self.offer.mmio_megabytes)
            .field("target_vp", target_vp)
            .field("guest_specified_event_flag", event_flag)
            .field("guest_specified_connection_id", connection_id)
            .field("reserved_connection_target", reserved_target)
            .binary("offer_flags", self.offer.flags.into_bits());
    }

    /// Returns the monitor ID and latency only if it's being handled by this server.
    ///
    /// The monitor ID can be set while use_mnf is Relayed, which is the case if
    /// the relay host is handling MNF.
    ///
    /// Also returns `None` for reserved channels, since monitored notifications
    /// are only usable for standard channels. Otherwise, we fail later when we
    /// try to change the MNF page as part of vmbus protocol renegotiation,
    /// since the page still appears to be in use by a device.
    fn handled_monitor_info(&self) -> Option<MonitorInfo> {
        self.offer.use_mnf.enabled_and_then(|latency| {
            if self.state.is_reserved() {
                None
            } else {
                self.info.and_then(|info| {
                    info.monitor_id.map(|monitor_id| MonitorInfo {
                        monitor_id,
                        latency,
                    })
                })
            }
        })
    }

    /// Prepares a channel to be sent to the guest by allocating a channel ID if
    /// necessary and filling out channel.info.
    fn prepare_channel(
        &mut self,
        offer_id: OfferId,
        assigned_channels: &mut AssignedChannels,
        assigned_monitors: &mut AssignedMonitors,
    ) {
        assert!(self.info.is_none());

        // Allocate a channel ID.
        let entry = assigned_channels
            .allocate()
            .expect("there are enough channel IDs for everything in ChannelList");

        let channel_id = entry.id();
        entry.insert(offer_id);
        let connection_id = ConnectionId::new(channel_id.0, assigned_channels.vtl, VMBUS_SINT);

        // Allocate a monitor ID if the channel uses MNF.
        // N.B. If the synic doesn't support MNF or MNF is disabled by the server, use_mnf should
        //      always be set to Disabled, except if the relay host is handling MnF in which case
        //      we should use the monitor ID it provided.
        let monitor_id = match self.offer.use_mnf {
            MnfUsage::Enabled { .. } => {
                let monitor_id = assigned_monitors.assign_monitor();
                if monitor_id.is_none() {
                    tracelimit::warn_ratelimited!("Out of monitor IDs.");
                }

                monitor_id
            }
            MnfUsage::Relayed { monitor_id } => Some(MonitorId(monitor_id)),
            MnfUsage::Disabled => None,
        };

        self.info = Some(OfferedInfo {
            channel_id,
            connection_id: connection_id.0,
            monitor_id,
        });
    }

    /// Releases a channel's ID.
    fn release_channel(
        &mut self,
        offer_id: OfferId,
        assigned_channels: &mut AssignedChannels,
        assigned_monitors: &mut AssignedMonitors,
    ) {
        if let Some(info) = self.info.take() {
            assigned_channels.free(info.channel_id, offer_id);

            // Only unassign the monitor ID if it was not a relayed ID provided by the offer.
            if let Some(monitor_id) = info.monitor_id {
                if self.offer.use_mnf.is_enabled() {
                    assigned_monitors.release_monitor(monitor_id);
                }
            }
        }
    }
}

#[derive(Debug)]
struct AssignedChannels {
    assignments: Vec<Option<OfferId>>,
    vtl: Vtl,
    reserved_offset: usize,
    /// The number of assigned channel IDs in the reserved range.
    count_in_reserved_range: usize,
}

impl AssignedChannels {
    fn new(vtl: Vtl, channel_id_offset: u16) -> Self {
        Self {
            assignments: vec![None; MAX_CHANNELS],
            vtl,
            reserved_offset: channel_id_offset as usize,
            count_in_reserved_range: 0,
        }
    }

    fn allowable_channel_count(&self) -> usize {
        MAX_CHANNELS - self.reserved_offset + self.count_in_reserved_range
    }

    fn get(&self, channel_id: ChannelId) -> Option<OfferId> {
        self.assignments
            .get(Self::index(channel_id))
            .copied()
            .flatten()
    }

    fn set(&mut self, channel_id: ChannelId) -> Result<AssignmentEntry<'_>, OfferError> {
        let index = Self::index(channel_id);
        if self
            .assignments
            .get(index)
            .ok_or(OfferError::InvalidChannelId(channel_id))?
            .is_some()
        {
            return Err(OfferError::ChannelIdInUse(channel_id));
        }
        Ok(AssignmentEntry { list: self, index })
    }

    fn allocate(&mut self) -> Option<AssignmentEntry<'_>> {
        let index = self.reserved_offset
            + self.assignments[self.reserved_offset..]
                .iter()
                .position(|x| x.is_none())?;
        Some(AssignmentEntry { list: self, index })
    }

    fn free(&mut self, channel_id: ChannelId, offer_id: OfferId) {
        let index = Self::index(channel_id);
        let slot = &mut self.assignments[index];
        assert_eq!(slot.take(), Some(offer_id));
        if index < self.reserved_offset {
            self.count_in_reserved_range -= 1;
        }
    }

    fn index(channel_id: ChannelId) -> usize {
        channel_id.0.wrapping_sub(1) as usize
    }
}

struct AssignmentEntry<'a> {
    list: &'a mut AssignedChannels,
    index: usize,
}

impl AssignmentEntry<'_> {
    pub fn id(&self) -> ChannelId {
        ChannelId(self.index as u32 + 1)
    }

    pub fn insert(self, offer_id: OfferId) {
        assert!(
            self.list.assignments[self.index]
                .replace(offer_id)
                .is_none()
        );

        if self.index < self.list.reserved_offset {
            self.list.count_in_reserved_range += 1;
        }
    }
}

struct ChannelList {
    channels: Slab<Channel>,
}

fn channel_inspect_path(offer: &OfferParamsInternal, suffix: std::fmt::Arguments<'_>) -> String {
    if offer.subchannel_index == 0 {
        format!("{}{}", offer.instance_id, suffix)
    } else {
        format!(
            "{}/subchannels/{}{}",
            offer.instance_id, offer.subchannel_index, suffix
        )
    }
}

impl ChannelList {
    fn inspect(
        &self,
        notifier: &impl Notifier,
        version: Option<VersionInfo>,
        resp: &mut inspect::Response<'_>,
    ) {
        for (offer_id, channel) in self.iter() {
            resp.child(
                &channel_inspect_path(&channel.offer, format_args!("")),
                |req| {
                    let mut resp = req.respond();
                    channel.inspect_state(&mut resp);

                    // Merge in the inspection state from outside. Skip this if
                    // the channel is revoked (and not reoffered) since in that
                    // case the caller won't recognize the channel ID.
                    resp.merge(inspect::adhoc(|req| {
                        if !matches!(channel.state, ChannelState::Revoked) {
                            notifier.inspect(version, offer_id, req);
                        }
                    }));
                },
            );
        }
    }
}

// This is limited by the size of the synic event flags bitmap (2048 bits per
// processor, bit 0 reserved for legacy channel bitmap multiplexing).
pub const MAX_CHANNELS: usize = 2047;

impl ChannelList {
    fn new() -> Self {
        Self {
            channels: Slab::new(),
        }
    }

    // The number of channels in the list.
    fn len(&self) -> usize {
        self.channels.len()
    }

    /// Inserts a channel.
    fn offer(&mut self, new_channel: Channel) -> OfferId {
        OfferId(self.channels.insert(new_channel))
    }

    /// Removes a channel by offer ID.
    fn remove(&mut self, offer_id: OfferId) {
        let channel = self.channels.remove(offer_id.0);
        assert!(channel.info.is_none());
    }

    /// Gets a channel by guest channel ID.
    fn get_by_channel_id_mut(
        &mut self,
        assigned_channels: &AssignedChannels,
        channel_id: ChannelId,
    ) -> Result<(OfferId, &mut Channel), ChannelError> {
        let offer_id = assigned_channels
            .get(channel_id)
            .ok_or(ChannelError::UnknownChannelId)?;
        let channel = &mut self[offer_id];
        if channel.state.is_released() {
            return Err(ChannelError::ChannelReleased);
        }
        assert_eq!(
            channel.info.as_ref().map(|info| info.channel_id),
            Some(channel_id)
        );
        Ok((offer_id, channel))
    }

    /// Gets a channel by guest channel ID.
    fn get_by_channel_id(
        &self,
        assigned_channels: &AssignedChannels,
        channel_id: ChannelId,
    ) -> Result<(OfferId, &Channel), ChannelError> {
        let offer_id = assigned_channels
            .get(channel_id)
            .ok_or(ChannelError::UnknownChannelId)?;
        let channel = &self[offer_id];
        if channel.state.is_released() {
            return Err(ChannelError::ChannelReleased);
        }
        assert_eq!(
            channel.info.as_ref().map(|info| info.channel_id),
            Some(channel_id)
        );
        Ok((offer_id, channel))
    }

    /// Gets a channel by offer key (interface ID, instance ID, subchannel
    /// index).
    fn get_by_key_mut(&mut self, key: &OfferKey) -> Option<(OfferId, &mut Channel)> {
        for (offer_id, channel) in self.iter_mut() {
            if channel.offer.instance_id == key.instance_id
                && channel.offer.interface_id == key.interface_id
                && channel.offer.subchannel_index == key.subchannel_index
            {
                return Some((offer_id, channel));
            }
        }
        None
    }

    /// Returns an iterator over the channels.
    fn iter(&self) -> impl Iterator<Item = (OfferId, &Channel)> {
        self.channels
            .iter()
            .map(|(id, channel)| (OfferId(id), channel))
    }

    /// Returns an iterator over the channels.
    fn iter_mut(&mut self) -> impl Iterator<Item = (OfferId, &mut Channel)> {
        self.channels
            .iter_mut()
            .map(|(id, channel)| (OfferId(id), channel))
    }

    /// Iterates through the channels, retaining those where `f` returns true.
    fn retain<F>(&mut self, mut f: F)
    where
        F: FnMut(OfferId, &mut Channel) -> bool,
    {
        self.channels.retain(|id, channel| {
            let retain = f(OfferId(id), channel);
            if !retain {
                assert!(channel.info.is_none());
            }
            retain
        })
    }
}

impl Index<OfferId> for ChannelList {
    type Output = Channel;

    fn index(&self, offer_id: OfferId) -> &Self::Output {
        &self.channels[offer_id.0]
    }
}

impl IndexMut<OfferId> for ChannelList {
    fn index_mut(&mut self, offer_id: OfferId) -> &mut Self::Output {
        &mut self.channels[offer_id.0]
    }
}

/// A GPADL.
#[derive(Debug, Inspect)]
struct Gpadl {
    count: u16,
    #[inspect(skip)]
    buf: Vec<u64>,
    state: GpadlState,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Inspect)]
enum GpadlState {
    /// The GPADL has not yet been fully sent to the host.
    InProgress,
    /// The GPADL has been sent to the device but is not yet acknowledged.
    Offered,
    /// The device has not acknowledged the GPADL but the GPADL is ready to be
    /// torn down.
    OfferedTearingDown,
    /// The device has acknowledged the GPADL.
    Accepted,
    /// The device has been notified that the GPADL is being torn down.
    TearingDown,
}

impl Gpadl {
    /// Creates a new GPADL with `count` ranges and `len * 8` bytes in the range
    /// buffer.
    fn new(count: u16, len: usize) -> Self {
        Self {
            state: GpadlState::InProgress,
            count,
            buf: Vec::with_capacity(len),
        }
    }

    /// Appends `data` to an in-progress GPADL. Returns whether the GPADL is complete.
    fn append(&mut self, data: &[u8]) -> Result<bool, ChannelError> {
        if self.state == GpadlState::InProgress {
            let buf = &mut self.buf;
            // data.len() may be longer than is actually valid since some
            // clients (e.g. UEFI) always pass the maximum message length. In
            // this case, calculate the useful length from the remaining
            // capacity instead.
            let len = min(data.len() & !7, (buf.capacity() - buf.len()) * 8);
            let data = &data[..len];
            let start = buf.len();
            buf.resize(buf.len() + data.len() / 8, 0);
            buf[start..].as_mut_bytes().copy_from_slice(data);
            Ok(if buf.len() == buf.capacity() {
                gparange::validate_gpa_ranges(self.count as usize, buf)
                    .map_err(ChannelError::InvalidGpaRange)?;
                self.state = GpadlState::Offered;
                true
            } else {
                false
            })
        } else {
            Err(ChannelError::GpadlAlreadyComplete)
        }
    }
}

/// The parameters provided by the guest when the channel is being opened.
#[derive(Debug, Copy, Clone)]
pub struct OpenParams {
    pub open_data: OpenData,
    pub connection_id: u32,
    pub event_flag: u16,
    pub monitor_info: Option<MonitorInfo>,
    pub flags: protocol::OpenChannelFlags,
    pub reserved_target: Option<ConnectionTarget>,
    pub channel_id: ChannelId,
}

impl OpenParams {
    fn from_request(
        info: &OfferedInfo,
        request: &OpenRequest,
        monitor_info: Option<MonitorInfo>,
        reserved_target: Option<ConnectionTarget>,
    ) -> Self {
        // Determine whether to use the alternate IDs.
        // N.B. If not specified, the regular IDs are stored as "alternate" in the OpenData.
        let (event_flag, connection_id) = if let Some(id) = request.guest_specified_interrupt_info {
            (id.event_flag, id.connection_id)
        } else {
            (info.channel_id.0 as u16, info.connection_id)
        };

        Self {
            open_data: OpenData {
                target_vp: request.target_vp,
                ring_offset: request.downstream_ring_buffer_page_offset,
                ring_gpadl_id: request.ring_buffer_gpadl_id,
                user_data: request.user_data,
                event_flag,
                connection_id,
            },
            connection_id,
            event_flag,
            monitor_info,
            flags: request.flags.with_unused(0),
            reserved_target,
            channel_id: info.channel_id,
        }
    }
}

/// A channel action, sent to the device when a channel state changes.
#[derive(Debug)]
pub enum Action {
    Open(OpenParams, VersionInfo),
    Close,
    Gpadl(GpadlId, u16, Vec<u64>),
    TeardownGpadl {
        gpadl_id: GpadlId,
        post_restore: bool,
    },
    Modify {
        target_vp: u32,
    },
}

/// The supported VMBus protocol versions.
static SUPPORTED_VERSIONS: &[Version] = &[
    Version::V1,
    Version::Win7,
    Version::Win8,
    Version::Win8_1,
    Version::Win10,
    Version::Win10Rs3_0,
    Version::Win10Rs3_1,
    Version::Win10Rs4,
    Version::Win10Rs5,
    Version::Iron,
    Version::Copper,
];

// Feature flags that are always supported.
// N.B. Confidential channels are conditionally supported if running in the paravisor.
const SUPPORTED_FEATURE_FLAGS: FeatureFlags = FeatureFlags::new()
    .with_guest_specified_signal_parameters(true)
    .with_channel_interrupt_redirection(true)
    .with_modify_connection(true)
    .with_client_id(true)
    .with_pause_resume(true)
    .with_server_specified_monitor_pages(true);

/// Trait for sending requests to devices and the guest.
pub trait Notifier: Send {
    /// Requests a channel action.
    fn notify(&mut self, offer_id: OfferId, action: Action);

    /// Forward an unhandled InitiateContact request to an external server.
    fn forward_unhandled(&mut self, request: InitiateContactRequest);

    /// Update server state with information from the connection, and optionally notify the relay.
    ///
    /// N.B. If `ModifyConnectionRequest::notify_relay` is true and the function does not return an
    /// error, the server expects `Server::complete_modify_connection()` to be called, regardless of
    /// whether or not there is a relay.
    fn modify_connection(&mut self, request: ModifyConnectionRequest) -> anyhow::Result<()>;

    /// Inspects a channel.
    fn inspect(&self, version: Option<VersionInfo>, offer_id: OfferId, req: inspect::Request<'_>) {
        let _ = (version, offer_id, req);
    }

    /// Sends a synic message to the guest.
    /// Returns true if the message was sent, and false if it must be retried.
    #[must_use]
    fn send_message(&mut self, message: &OutgoingMessage, target: MessageTarget) -> bool;

    /// Used to signal the hvsocket handler that there is a new connection request.
    fn notify_hvsock(&mut self, request: &HvsockConnectRequest);

    /// Notifies that a requested reset is complete.
    fn reset_complete(&mut self);

    /// Notifies that a guest-requested unload is complete.
    fn unload_complete(&mut self);
}

impl Server {
    /// Creates a new VMBus server.
    pub fn new(
        vtl: Vtl,
        child_connection_id: u32,
        channel_id_offset: u16,
        use_absolute_channel_order: bool,
    ) -> Self {
        Server {
            state: ConnectionState::Disconnected,
            channels: ChannelList::new(),
            assigned_channels: AssignedChannels::new(vtl, channel_id_offset),
            assigned_monitors: AssignedMonitors::new(),
            gpadls: Default::default(),
            incomplete_gpadls: Default::default(),
            child_connection_id,
            max_version: None,
            delayed_max_version: None,
            pending_messages: PendingMessages(VecDeque::new()),
            require_server_allocated_mnf: false,
            use_absolute_channel_order,
        }
    }

    /// Associates a `Notifier` with the server.
    pub fn with_notifier<'a, T: Notifier>(
        &'a mut self,
        notifier: &'a mut T,
    ) -> ServerWithNotifier<'a, T> {
        self.validate();
        ServerWithNotifier {
            inner: self,
            notifier,
        }
    }

    /// Requires that the server allocates monitor pages. If this is enabled, the server will ignore
    /// guest-specified monitor pages and act as if none of the channels use MNF.
    pub fn set_require_server_allocated_mnf(&mut self, require: bool) {
        self.require_server_allocated_mnf = require;
    }

    fn validate(&self) {
        #[cfg(debug_assertions)]
        for (_, channel) in self.channels.iter() {
            let should_have_info = !channel.state.is_released();
            if channel.info.is_some() != should_have_info {
                panic!("channel invariant violation: {channel:?}");
            }
        }
    }

    /// Indicates the maximum supported version by the real host in an Underhill relay scenario.
    pub fn set_compatibility_version(&mut self, version: MaxVersionInfo, delay: bool) {
        if delay {
            self.delayed_max_version = Some(version)
        } else {
            tracing::info!(?version, "Limiting VmBus connections to version");
            self.max_version = Some(version);
        }
    }

    pub fn channel_gpadls(&self, offer_id: OfferId) -> Vec<RestoredGpadl> {
        self.gpadls
            .iter()
            .filter_map(|(&(gpadl_id, gpadl_offer_id), gpadl)| {
                if offer_id != gpadl_offer_id {
                    return None;
                }
                let accepted = match gpadl.state {
                    GpadlState::Offered | GpadlState::OfferedTearingDown => false,
                    GpadlState::Accepted => true,
                    GpadlState::InProgress | GpadlState::TearingDown => return None,
                };
                Some(RestoredGpadl {
                    request: GpadlRequest {
                        id: gpadl_id,
                        count: gpadl.count,
                        buf: gpadl.buf.clone(),
                    },
                    accepted,
                })
            })
            .collect()
    }

    pub fn get_version(&self) -> Option<VersionInfo> {
        self.state.get_version()
    }

    pub fn get_restore_open_params(&self, offer_id: OfferId) -> Result<OpenParams, RestoreError> {
        let channel = &self.channels[offer_id];

        // Check this here to avoid doing unnecessary work.
        match channel.restore_state {
            RestoreState::New => {
                // This channel was never offered, or was released by the guest during the save.
                // This is a problem since if this was called the device expects the channel to be
                // open.
                return Err(RestoreError::MissingChannel(channel.offer.key()));
            }
            RestoreState::Restoring => {}
            RestoreState::Unmatched => unreachable!(),
            RestoreState::Restored => {
                return Err(RestoreError::AlreadyRestored(channel.offer.key()));
            }
        }

        let info = channel
            .info
            .ok_or_else(|| RestoreError::MissingChannel(channel.offer.key()))?;

        let (request, reserved_state) = match channel.state {
            ChannelState::Closed => {
                return Err(RestoreError::MismatchedOpenState(channel.offer.key()));
            }
            ChannelState::Closing { params, .. } | ChannelState::ClosingReopen { params, .. } => {
                (params, None)
            }
            ChannelState::Opening {
                request,
                reserved_state,
            } => (request, reserved_state),
            ChannelState::Open {
                params,
                reserved_state,
                ..
            } => (params, reserved_state),
            ChannelState::ClientReleased | ChannelState::Reoffered => {
                return Err(RestoreError::MissingChannel(channel.offer.key()));
            }
            ChannelState::Revoked
            | ChannelState::ClosingClientRelease
            | ChannelState::OpeningClientRelease => unreachable!(),
        };

        Ok(OpenParams::from_request(
            &info,
            &request,
            channel.handled_monitor_info(),
            reserved_state.map(|state| state.target),
        ))
    }

    /// Check if there are any messages in the pending queue.
    pub fn has_pending_messages(&self) -> bool {
        !self.pending_messages.0.is_empty() && !self.state.is_paused()
    }

    /// Tries to resend pending messages using the provided `send`` function.
    pub fn poll_flush_pending_messages(
        &mut self,
        mut send: impl FnMut(&OutgoingMessage) -> Poll<()>,
    ) -> Poll<()> {
        if !self.state.is_paused() {
            while let Some(message) = self.pending_messages.0.front() {
                ready!(send(message));
                self.pending_messages.0.pop_front();
            }
        }

        Poll::Ready(())
    }
}

impl<'a, N: 'a + Notifier> ServerWithNotifier<'a, N> {
    /// Marks a channel as restored.
    ///
    /// If this is not called for a channel but vmbus state is restored, then it
    /// is assumed that the offer is a fresh one, and the channel will be
    /// revoked and reoffered.
    pub fn restore_channel(&mut self, offer_id: OfferId, open: bool) -> Result<(), RestoreError> {
        let channel = &mut self.inner.channels[offer_id];

        // We need to check this here as well, because get_restore_open_params may not have been
        // called.
        match channel.restore_state {
            RestoreState::New => {
                // This channel was never offered, or was released by the guest
                // during the save. This is fine as long as the device does not
                // expect the channel to be open.
                if open {
                    return Err(RestoreError::MissingChannel(channel.offer.key()));
                } else {
                    return Ok(());
                }
            }
            RestoreState::Restoring => {}
            RestoreState::Unmatched => unreachable!(),
            RestoreState::Restored => {
                return Err(RestoreError::AlreadyRestored(channel.offer.key()));
            }
        }

        let info = channel
            .info
            .ok_or_else(|| RestoreError::MissingChannel(channel.offer.key()))?;

        if let Some(monitor_info) = channel.handled_monitor_info() {
            if !self
                .inner
                .assigned_monitors
                .claim_monitor(monitor_info.monitor_id)
            {
                return Err(RestoreError::DuplicateMonitorId(monitor_info.monitor_id.0));
            }
        }

        if open {
            match channel.state {
                ChannelState::Closed => {
                    return Err(RestoreError::MismatchedOpenState(channel.offer.key()));
                }
                ChannelState::Closing { .. } | ChannelState::ClosingReopen { .. } => {
                    self.notifier.notify(offer_id, Action::Close);
                }
                ChannelState::Opening {
                    request,
                    reserved_state,
                } => {
                    self.inner
                        .pending_messages
                        .sender(self.notifier, self.inner.state.is_paused())
                        .send_open_result(
                            info.channel_id,
                            &request,
                            protocol::STATUS_SUCCESS,
                            MessageTarget::for_offer(offer_id, &reserved_state),
                        );
                    channel.state = ChannelState::Open {
                        params: request,
                        modify_state: ModifyState::NotModifying,
                        reserved_state,
                    };
                }
                ChannelState::Open { .. } => {}
                ChannelState::ClientReleased | ChannelState::Reoffered => {
                    return Err(RestoreError::MissingChannel(channel.offer.key()));
                }
                ChannelState::Revoked
                | ChannelState::ClosingClientRelease
                | ChannelState::OpeningClientRelease => unreachable!(),
            };
        } else {
            match channel.state {
                ChannelState::Closed => {}
                // If a channel was reoffered before the save, it was saved as revoked and then
                // restored to reoffered if the device is offering it again. If we reach this state,
                // the device has offered the channel but we are still waiting for the client to
                // release the old revoked channel, so the state must remain reoffered.
                ChannelState::Reoffered => {}
                ChannelState::Closing { .. } => {
                    channel.state = ChannelState::Closed;
                }
                ChannelState::ClosingReopen { request, .. } => {
                    self.notifier.notify(
                        offer_id,
                        Action::Open(
                            OpenParams::from_request(
                                &info,
                                &request,
                                channel.handled_monitor_info(),
                                None,
                            ),
                            self.inner.state.get_version().expect("must be connected"),
                        ),
                    );
                    channel.state = ChannelState::Opening {
                        request,
                        reserved_state: None,
                    };
                }
                ChannelState::Opening {
                    request,
                    reserved_state,
                } => {
                    self.notifier.notify(
                        offer_id,
                        Action::Open(
                            OpenParams::from_request(
                                &info,
                                &request,
                                channel.handled_monitor_info(),
                                reserved_state.map(|state| state.target),
                            ),
                            self.inner.state.get_version().expect("must be connected"),
                        ),
                    );
                }
                ChannelState::Open { .. } => {
                    return Err(RestoreError::MismatchedOpenState(channel.offer.key()));
                }
                ChannelState::ClientReleased => {
                    return Err(RestoreError::MissingChannel(channel.offer.key()));
                }
                ChannelState::Revoked
                | ChannelState::ClosingClientRelease
                | ChannelState::OpeningClientRelease => unreachable!(),
            }
        }

        channel.restore_state = RestoreState::Restored;
        Ok(())
    }

    /// Revoke and reoffer channels to the guest, depending on their `RestoreState.`
    /// This function should be called after [`ServerWithNotifier::restore`].
    pub fn revoke_unclaimed_channels(&mut self) {
        for (offer_id, channel) in self.inner.channels.iter_mut() {
            match channel.restore_state {
                RestoreState::Restored => {
                    // The channel is fully restored. Nothing more to do.
                }
                RestoreState::New => {
                    // This is a fresh channel offer, not in the saved state. Send the offer to the
                    // guest if it has not already been sent (which could have happened if the
                    // channel was offered after restore() but before revoke_unclaimed_channels()).
                    // Offers should only be sent if the guest has already sent RequestOffers.
                    if let ConnectionState::Connected(info) = &self.inner.state {
                        if info.offers_sent && matches!(channel.state, ChannelState::ClientReleased)
                        {
                            channel.prepare_channel(
                                offer_id,
                                &mut self.inner.assigned_channels,
                                &mut self.inner.assigned_monitors,
                            );
                            channel.state = ChannelState::Closed;
                            self.inner
                                .pending_messages
                                .sender(self.notifier, self.inner.state.is_paused())
                                .send_offer(channel, info);
                        }
                    }
                }
                RestoreState::Restoring => {
                    // restore_channel was never called for this, but it was in
                    // the saved state. This indicates the offer is meant to be
                    // fresh, so revoke and reoffer it.
                    let retain = revoke(
                        self.inner
                            .pending_messages
                            .sender(self.notifier, self.inner.state.is_paused()),
                        offer_id,
                        channel,
                        &mut self.inner.gpadls,
                    );
                    assert!(retain, "channel has not been released");
                    channel.state = ChannelState::Reoffered;
                }
                RestoreState::Unmatched => {
                    // offer_channel was never called for this, but it was in
                    // the saved state. Revoke it.
                    let retain = revoke(
                        self.inner
                            .pending_messages
                            .sender(self.notifier, self.inner.state.is_paused()),
                        offer_id,
                        channel,
                        &mut self.inner.gpadls,
                    );
                    assert!(retain, "channel has not been released");
                }
            }
        }

        // Notify the channels for any GPADLs in progress.
        for (&(gpadl_id, offer_id), gpadl) in self.inner.gpadls.iter_mut() {
            match gpadl.state {
                GpadlState::InProgress | GpadlState::Accepted => {}
                GpadlState::Offered => {
                    self.notifier.notify(
                        offer_id,
                        Action::Gpadl(gpadl_id, gpadl.count, gpadl.buf.clone()),
                    );
                }
                GpadlState::TearingDown => {
                    self.notifier.notify(
                        offer_id,
                        Action::TeardownGpadl {
                            gpadl_id,
                            post_restore: true,
                        },
                    );
                }
                GpadlState::OfferedTearingDown => unreachable!(),
            }
        }

        self.check_disconnected();
    }

    /// Initiates a state reset and a closing of all channels.
    ///
    /// Only one reset is allowed at a time, and no calls to
    /// `handle_synic_message` are allowed during a reset operation.
    pub fn reset(&mut self) {
        assert!(!self.is_resetting());
        if self.request_disconnect(ConnectionAction::Reset) {
            self.complete_reset();
        }
    }

    fn complete_reset(&mut self) {
        // Reset the restore state since everything is now in a clean state.
        for (_, channel) in self.inner.channels.iter_mut() {
            channel.restore_state = RestoreState::New;
        }
        self.inner.pending_messages.0.clear();
        self.notifier.reset_complete();
    }

    /// Creates a new channel, returning its offer ID.
    pub fn offer_channel(&mut self, offer: OfferParamsInternal) -> Result<OfferId, OfferError> {
        // Ensure no channel with this interface and instance ID exists.
        if let Some((offer_id, channel)) = self.inner.channels.get_by_key_mut(&offer.key()) {
            // Replace the current offer if this is an unmatched restored
            // channel, or if this matching offer has been revoked by the host
            // but not yet released by the guest.
            if channel.restore_state != RestoreState::Unmatched
                && !matches!(channel.state, ChannelState::Revoked)
            {
                return Err(OfferError::AlreadyExists(offer.key()));
            }

            let info = channel.info.expect("assigned");
            if channel.restore_state == RestoreState::Unmatched {
                tracing::debug!(
                    offer_id = offer_id.0,
                    key = %channel.offer.key(),
                    "matched channel"
                );

                assert!(!matches!(channel.state, ChannelState::Revoked));
                // This channel was previously offered to the guest in the saved
                // state. Match this back up to handle future calls to
                // restore_channel and revoke_unclaimed_channels.
                channel.restore_state = RestoreState::Restoring;

                // The relay can specify a host-determined monitor ID, which needs to match what's
                // in the saved state.
                if let MnfUsage::Relayed { monitor_id } = offer.use_mnf {
                    if info.monitor_id != Some(MonitorId(monitor_id)) {
                        return Err(OfferError::MismatchedMonitorId(
                            info.monitor_id,
                            MonitorId(monitor_id),
                        ));
                    }
                }
            } else {
                // The channel has been revoked but the guest still has a
                // reference to it. Save the offer for reoffering immediately
                // after the child releases it.
                channel.state = ChannelState::Reoffered;
                tracing::info!(?offer_id, key = %channel.offer.key(), "channel marked for reoffer");
            }

            channel.offer = offer;
            return Ok(offer_id);
        }

        let mut connected_info = None;
        let state = match &self.inner.state {
            ConnectionState::Connected(info) => {
                if info.offers_sent {
                    connected_info = Some(info);
                    ChannelState::Closed
                } else {
                    ChannelState::ClientReleased
                }
            }
            ConnectionState::Connecting { .. }
            | ConnectionState::Disconnecting { .. }
            | ConnectionState::Disconnected => ChannelState::ClientReleased,
        };

        // Ensure there will be enough channel IDs for this channel.
        if self.inner.channels.len() >= self.inner.assigned_channels.allowable_channel_count() {
            return Err(OfferError::TooManyChannels);
        }

        let key = offer.key();
        let confidential_ring_buffer = offer.flags.confidential_ring_buffer();
        let confidential_external_memory = offer.flags.confidential_external_memory();
        let channel = Channel {
            info: None,
            offer,
            state,
            restore_state: RestoreState::New,
        };

        let offer_id = self.inner.channels.offer(channel);
        if let Some(info) = connected_info {
            let channel = &mut self.inner.channels[offer_id];
            channel.prepare_channel(
                offer_id,
                &mut self.inner.assigned_channels,
                &mut self.inner.assigned_monitors,
            );

            self.inner
                .pending_messages
                .sender(self.notifier, self.inner.state.is_paused())
                .send_offer(channel, info);
        }

        tracing::info!(?offer_id, %key, confidential_ring_buffer, confidential_external_memory, "new channel");
        Ok(offer_id)
    }

    /// Revokes a channel by ID.
    pub fn revoke_channel(&mut self, offer_id: OfferId) {
        let channel = &mut self.inner.channels[offer_id];
        let retain = revoke(
            self.inner
                .pending_messages
                .sender(self.notifier, self.inner.state.is_paused()),
            offer_id,
            channel,
            &mut self.inner.gpadls,
        );
        if !retain {
            self.inner.channels.remove(offer_id);
        }

        self.check_disconnected();
    }

    /// Completes an open operation with `result`.
    pub fn open_complete(&mut self, offer_id: OfferId, result: i32) {
        let channel = &mut self.inner.channels[offer_id];
        tracing::debug!(offer_id = offer_id.0, key = %channel.offer.key(), result, "open complete");

        match channel.state {
            ChannelState::Opening {
                request,
                reserved_state,
            } => {
                let channel_id = channel.info.expect("assigned").channel_id;
                if result >= 0 {
                    tracelimit::info_ratelimited!(
                        offer_id = offer_id.0,
                        channel_id = channel_id.0,
                        key = %channel.offer.key(),
                        result,
                        "opened channel"
                    );
                } else {
                    // Log channel open failures at error level for visibility.
                    tracelimit::error_ratelimited!(
                        offer_id = offer_id.0,
                        channel_id = channel_id.0,
                        key = %channel.offer.key(),
                        result,
                        "failed to open channel"
                    );
                }

                self.inner
                    .pending_messages
                    .sender(self.notifier, self.inner.state.is_paused())
                    .send_open_result(
                        channel_id,
                        &request,
                        result,
                        MessageTarget::for_offer(offer_id, &reserved_state),
                    );
                channel.state = if result >= 0 {
                    ChannelState::Open {
                        params: request,
                        modify_state: ModifyState::NotModifying,
                        reserved_state,
                    }
                } else {
                    ChannelState::Closed
                };
            }
            ChannelState::OpeningClientRelease => {
                tracing::info!(
                    offer_id = offer_id.0,
                    key = %channel.offer.key(),
                    result,
                    "opened channel (client released)"
                );

                if result >= 0 {
                    channel.state = ChannelState::ClosingClientRelease;
                    self.notifier.notify(offer_id, Action::Close);
                } else {
                    channel.state = ChannelState::ClientReleased;
                    self.check_disconnected();
                }
            }

            ChannelState::ClientReleased
            | ChannelState::Closed
            | ChannelState::Open { .. }
            | ChannelState::Closing { .. }
            | ChannelState::ClosingReopen { .. }
            | ChannelState::Revoked
            | ChannelState::Reoffered
            | ChannelState::ClosingClientRelease => {
                tracing::error!(?offer_id, key = %channel.offer.key(), state = ?channel.state, "invalid open complete")
            }
        }
    }

    /// If true, all channels are in a reset state, with no references by the
    /// guest. Reserved channels should only be included if the VM is resetting.
    fn are_channels_reset(&self, include_reserved: bool) -> bool {
        self.inner.gpadls.keys().all(|(_, offer_id)| {
            !include_reserved && self.inner.channels[*offer_id].state.is_reserved()
        }) && self.inner.channels.iter().all(|(_, channel)| {
            matches!(channel.state, ChannelState::ClientReleased)
                || (!include_reserved && channel.state.is_reserved())
        })
    }

    /// Checks if the connection state is fully disconnected and advances the
    /// connection state machine. Must be called any time a GPADL is deleted or
    /// a channel enters the ClientReleased state.
    fn check_disconnected(&mut self) {
        match self.inner.state {
            ConnectionState::Disconnecting {
                next_action,
                modify_sent: false,
            } => {
                if self.are_channels_reset(matches!(next_action, ConnectionAction::Reset)) {
                    self.notify_disconnect(next_action);
                }
            }
            ConnectionState::Disconnecting {
                modify_sent: true, ..
            }
            | ConnectionState::Disconnected
            | ConnectionState::Connected { .. }
            | ConnectionState::Connecting { .. } => (),
        }
    }

    /// Informs the notifier to reset the connection state when disconnecting.
    fn notify_disconnect(&mut self, next_action: ConnectionAction) {
        // Assert this on debug only because it is an expensive check if there are many channels.
        debug_assert!(self.are_channels_reset(matches!(next_action, ConnectionAction::Reset)));
        self.inner.state = ConnectionState::Disconnecting {
            next_action,
            modify_sent: true,
        };

        // Reset server state and disconnect the relay if there is one.
        self.notifier
            .modify_connection(ModifyConnectionRequest {
                monitor_page: Update::Reset,
                interrupt_page: Update::Reset,
                ..Default::default()
            })
            .expect("resetting state should not fail");
    }

    /// If true, the server is mid-reset and cannot take certain actions such
    /// as handling synic messages or saving state.
    fn is_resetting(&self) -> bool {
        matches!(
            &self.inner.state,
            ConnectionState::Connecting {
                next_action: ConnectionAction::Reset,
                ..
            } | ConnectionState::Disconnecting {
                next_action: ConnectionAction::Reset,
                ..
            }
        )
    }

    /// Completes a channel close operation.
    pub fn close_complete(&mut self, offer_id: OfferId) {
        let channel = &mut self.inner.channels[offer_id];
        tracing::info!(offer_id = offer_id.0, key = %channel.offer.key(), "closed channel");
        match channel.state {
            ChannelState::Closing {
                reserved_state: Some(reserved_state),
                ..
            } => {
                channel.state = ChannelState::Closed;
                if matches!(self.inner.state, ConnectionState::Connected { .. }) {
                    let channel_id = channel.info.expect("assigned").channel_id;
                    self.send_close_reserved_channel_response(
                        channel_id,
                        offer_id,
                        reserved_state.target,
                    );
                } else {
                    // Handle closing reserved channels while disconnected/ing. Since we weren't waiting
                    // on the channel, no need to call check_disconnected, but we do need to release it.
                    if Self::client_release_channel(
                        self.inner
                            .pending_messages
                            .sender(self.notifier, self.inner.state.is_paused()),
                        offer_id,
                        channel,
                        &mut self.inner.gpadls,
                        &mut self.inner.incomplete_gpadls,
                        &mut self.inner.assigned_channels,
                        &mut self.inner.assigned_monitors,
                        None,
                    ) {
                        self.inner.channels.remove(offer_id);
                    }
                }
            }
            ChannelState::Closing { .. } => {
                channel.state = ChannelState::Closed;
            }
            ChannelState::ClosingClientRelease => {
                channel.state = ChannelState::ClientReleased;
                self.check_disconnected();
            }
            ChannelState::ClosingReopen { request, .. } => {
                channel.state = ChannelState::Closed;
                self.open_channel(offer_id, &request, None);
            }

            ChannelState::Closed
            | ChannelState::ClientReleased
            | ChannelState::Opening { .. }
            | ChannelState::Open { .. }
            | ChannelState::Revoked
            | ChannelState::Reoffered
            | ChannelState::OpeningClientRelease => {
                tracing::error!(?offer_id, key = %channel.offer.key(), state = ?channel.state, "invalid close complete")
            }
        }
    }

    fn send_close_reserved_channel_response(
        &mut self,
        channel_id: ChannelId,
        offer_id: OfferId,
        target: ConnectionTarget,
    ) {
        self.sender().send_message_with_target(
            &protocol::CloseReservedChannelResponse { channel_id },
            MessageTarget::ReservedChannel(offer_id, target),
        );
    }

    /// Handles MessageType::INITIATE_CONTACT, which requests version
    /// negotiation.
    fn handle_initiate_contact(
        &mut self,
        input: &protocol::InitiateContact2,
        message: &SynicMessage,
        includes_client_id: bool,
    ) -> Result<(), ChannelError> {
        let target_info =
            protocol::TargetInfo::from(input.initiate_contact.interrupt_page_or_target_info);

        let target_sint = if message.multiclient
            && input.initiate_contact.version_requested >= Version::Win10Rs3_1 as u32
        {
            target_info.sint()
        } else {
            VMBUS_SINT
        };

        let target_vtl = if message.multiclient
            && input.initiate_contact.version_requested >= Version::Win10Rs4 as u32
        {
            target_info.vtl()
        } else {
            0
        };

        let feature_flags = if input.initiate_contact.version_requested >= Version::Copper as u32 {
            target_info.feature_flags()
        } else {
            0
        };

        // Originally, messages were always sent to processor zero.
        // Post-Windows 8, it became necessary to send messages to other
        // processors in order to support establishing channel connections
        // on arbitrary processors after crashing.
        let target_message_vp =
            if input.initiate_contact.version_requested >= Version::Win8_1 as u32 {
                input.initiate_contact.target_message_vp
            } else {
                0
            };

        // Guests can send an interrupt page up to protocol Win10Rs3_1 (at which point the
        // interrupt page field was reused), but as of Win8 the host can ignore it as it won't be
        // used for channels with dedicated interrupts (which is all channels).
        //
        // V1 doesn't support dedicated interrupts and Win7 only uses dedicated interrupts for
        // guest-to-host, so the interrupt page is still used for host-to-guest.
        let interrupt_page = (input.initiate_contact.version_requested < Version::Win8 as u32
            && input.initiate_contact.interrupt_page_or_target_info != 0)
            .then_some(input.initiate_contact.interrupt_page_or_target_info);

        // The guest must specify both monitor pages, or neither. Store this information in the
        // request so the response can be sent after the version check, and to the correct VTL.
        let monitor_page = if (input.initiate_contact.parent_to_child_monitor_page_gpa == 0)
            != (input.initiate_contact.child_to_parent_monitor_page_gpa == 0)
        {
            MonitorPageRequest::Invalid
        } else if input.initiate_contact.parent_to_child_monitor_page_gpa != 0 {
            MonitorPageRequest::Some(MonitorPageGpas {
                parent_to_child: input.initiate_contact.parent_to_child_monitor_page_gpa,
                child_to_parent: input.initiate_contact.child_to_parent_monitor_page_gpa,
            })
        } else {
            MonitorPageRequest::None
        };

        // We differentiate between InitiateContact and InitiateContact2 only by size, so we need to
        // check the feature flags here to ensure the client ID should actually be set to the input GUID.
        let client_id = if FeatureFlags::from(feature_flags).client_id() {
            if includes_client_id {
                input.client_id
            } else {
                return Err(ChannelError::ParseError(
                    protocol::ParseError::MessageTooSmall(Some(
                        protocol::MessageType::INITIATE_CONTACT,
                    )),
                ));
            }
        } else {
            Guid::ZERO
        };

        let request = InitiateContactRequest {
            version_requested: input.initiate_contact.version_requested,
            target_message_vp,
            monitor_page,
            target_sint,
            target_vtl,
            feature_flags,
            interrupt_page,
            client_id,
            trusted: message.trusted,
        };
        self.initiate_contact(request);
        Ok(())
    }

    pub fn initiate_contact(&mut self, request: InitiateContactRequest) {
        // If the request is not for this server's VTL, inform the notifier it wasn't handled so it
        // can be forwarded to the correct server.
        let vtl = self.inner.assigned_channels.vtl as u8;
        if request.target_vtl != vtl {
            // Send a notification to a linked server (which handles a different VTL).
            self.notifier.forward_unhandled(request);
            return;
        }

        if request.target_sint != VMBUS_SINT {
            tracelimit::warn_ratelimited!(
                target_vtl = request.target_vtl,
                target_sint = request.target_sint,
                version = request.version_requested,
                "unsupported multiclient request",
            );

            // Send an unsupported response to the requested SINT.
            self.send_version_response_with_target(
                None,
                MessageTarget::Custom(ConnectionTarget {
                    vp: request.target_message_vp,
                    sint: request.target_sint,
                }),
            );

            return;
        }

        if !self.request_disconnect(ConnectionAction::Reconnect {
            initiate_contact: request,
        }) {
            return;
        }

        let Some(version) = self.check_version_supported(&request) else {
            tracelimit::warn_ratelimited!(
                vtl,
                version = request.version_requested,
                client_id = ?request.client_id,
                "Guest requested unsupported version"
            );

            // Do not notify the relay in this case.
            self.send_version_response(None);
            return;
        };

        tracelimit::info_ratelimited!(
            vtl,
            ?version,
            client_id = ?request.client_id,
            trusted = request.trusted,
            "Guest negotiated version"
        );

        // Make sure we can receive incoming interrupts on the monitor page. The parent to child
        // page is not used as this server doesn't send monitored interrupts.
        let monitor_page = match request.monitor_page {
            MonitorPageRequest::Some(mp) => {
                if self.inner.require_server_allocated_mnf {
                    if !version.feature_flags.server_specified_monitor_pages() {
                        tracelimit::warn_ratelimited!(
                            "guest-supplied monitor pages not supported; MNF will be disabled"
                        );
                    }

                    None
                } else {
                    Some(mp)
                }
            }
            MonitorPageRequest::None => None,
            MonitorPageRequest::Invalid => {
                // Do not notify the relay in this case.
                self.send_version_response(Some(VersionResponseData::new(
                    version,
                    protocol::ConnectionState::FAILED_UNKNOWN_FAILURE,
                )));

                return;
            }
        };

        self.inner.state = ConnectionState::Connecting {
            info: ConnectionInfo {
                version,
                trusted: request.trusted,
                interrupt_page: request.interrupt_page,
                monitor_page: monitor_page.map(MonitorPageGpaInfo::from_guest_gpas),
                target_message_vp: request.target_message_vp,
                modifying: false,
                offers_sent: false,
                client_id: request.client_id,
                paused: false,
            },
            next_action: ConnectionAction::None,
        };

        // Update server state and notify the relay, if any. When complete,
        // complete_initiate_contact will be invoked.
        if let Err(err) = self.notifier.modify_connection(ModifyConnectionRequest {
            version: Some(version),
            monitor_page: monitor_page.into(),
            interrupt_page: request.interrupt_page.into(),
            target_message_vp: Some(request.target_message_vp),
            notify_relay: true,
        }) {
            tracelimit::error_ratelimited!(?err, "server failed to change state");
            self.inner.state = ConnectionState::Disconnected;
            self.send_version_response(Some(VersionResponseData::new(
                version,
                protocol::ConnectionState::FAILED_UNKNOWN_FAILURE,
            )));
        }
    }

    pub(crate) fn complete_initiate_contact(&mut self, response: ModifyConnectionResponse) {
        let ConnectionState::Connecting {
            mut info,
            next_action,
        } = self.inner.state
        else {
            panic!("Invalid state for completing InitiateContact.");
        };

        // Some features are handled locally without needing relay support.
        // N.B. Server-specified monitor pages are also handled locally but are only conditionally
        //      supported.
        const LOCAL_FEATURE_FLAGS: FeatureFlags = FeatureFlags::new()
            .with_client_id(true)
            .with_confidential_channels(true);

        let (relay_feature_flags, server_specified_monitor_page) = match response {
            // There is no relay, or it successfully processed our request.
            ModifyConnectionResponse::Supported(
                protocol::ConnectionState::SUCCESSFUL,
                feature_flags,
                server_specified_monitor_page,
            ) => (feature_flags, server_specified_monitor_page),
            // The relay supports the requested version, but encountered an error, so pass it
            // along to the guest.
            ModifyConnectionResponse::Supported(
                connection_state,
                feature_flags,
                server_specified_monitor_page,
            ) => {
                tracelimit::error_ratelimited!(
                    ?connection_state,
                    "initiate contact failed because relay request failed"
                );

                // We still report the supported feature flags with an error, so make sure those
                // are correct.
                info.version.feature_flags &= (feature_flags | LOCAL_FEATURE_FLAGS)
                    .with_server_specified_monitor_pages(server_specified_monitor_page.is_some());

                self.send_version_response(Some(VersionResponseData::new(
                    info.version,
                    connection_state,
                )));
                self.inner.state = ConnectionState::Disconnected;
                return;
            }
            // The relay doesn't support the requested version, so tell the guest to negotiate a new
            // one.
            ModifyConnectionResponse::Unsupported => {
                self.send_version_response(None);
                self.inner.state = ConnectionState::Disconnected;
                return;
            }
            ModifyConnectionResponse::Modified(_) => {
                panic!("Invalid response for completing InitiateContact.");
            }
        };

        // The server may not provide its own monitor pages if the guest didn't request them.
        assert!(
            info.version.feature_flags.server_specified_monitor_pages()
                || server_specified_monitor_page.is_none()
        );

        // The relay responds with all the feature flags it supports, so limit the flags reported to
        // the guest to include only those handled by the relay or locally.
        info.version.feature_flags &= relay_feature_flags | LOCAL_FEATURE_FLAGS;

        // If the server allocated a monitor page, also report that feature is supported, and store
        // the server pages. The feature bit must be re-enabled because the relay may not report
        // support for it.
        if let Some(gpas) = server_specified_monitor_page {
            info.monitor_page = Some(MonitorPageGpaInfo::from_server_gpas(gpas));
            info.version
                .feature_flags
                .set_server_specified_monitor_pages(true);
        } else {
            info.version
                .feature_flags
                .set_server_specified_monitor_pages(false);
        }

        let version = info.version;
        self.inner.state = ConnectionState::Connected(info);

        self.send_version_response(Some(
            VersionResponseData::new(version, protocol::ConnectionState::SUCCESSFUL)
                .with_monitor_pages(server_specified_monitor_page),
        ));
        if !matches!(next_action, ConnectionAction::None) && self.request_disconnect(next_action) {
            self.do_next_action(next_action);
        }
    }

    /// Determine if a guest's requested version and feature flags are supported.
    fn check_version_supported(&self, request: &InitiateContactRequest) -> Option<VersionInfo> {
        let version = SUPPORTED_VERSIONS
            .iter()
            .find(|v| request.version_requested == **v as u32)
            .copied()?;

        // The max version may be limited in order to test older protocol versions.
        if let Some(max_version) = self.inner.max_version {
            if version as u32 > max_version.version {
                return None;
            }
        }

        let supported_flags = if version >= Version::Copper {
            // Confidential channels should only be enabled if the connection is trusted.
            let max_supported_flags =
                SUPPORTED_FEATURE_FLAGS.with_confidential_channels(request.trusted);

            // The max features may be limited in order to test older protocol versions.
            if let Some(max_version) = self.inner.max_version {
                max_supported_flags & max_version.feature_flags
            } else {
                max_supported_flags
            }
        } else {
            FeatureFlags::new()
        };

        let feature_flags = supported_flags & request.feature_flags.into();

        assert!(version >= Version::Copper || feature_flags == FeatureFlags::new());
        if feature_flags.into_bits() != request.feature_flags {
            tracelimit::warn_ratelimited!(
                supported = feature_flags.into_bits(),
                requested = request.feature_flags,
                "Guest requested unsupported feature flags."
            );
        }

        Some(VersionInfo {
            version,
            feature_flags,
        })
    }

    fn send_version_response(&mut self, data: Option<VersionResponseData>) {
        self.send_version_response_with_target(data, MessageTarget::Default);
    }

    fn send_version_response_with_target(
        &mut self,
        data: Option<VersionResponseData>,
        target: MessageTarget,
    ) {
        enum VersionResponseType {
            PreCopper,
            Copper,
            CopperWithServerMnf,
        }

        let mut response_copper_with_mnf = protocol::VersionResponse3::new_zeroed();
        let response_copper = &mut response_copper_with_mnf.version_response2;
        let response = &mut response_copper.version_response;
        let mut response_type = VersionResponseType::PreCopper;
        if let Some(data) = data {
            // Pre-Win8, there is no way to report failures to the guest, so those should be treated
            // as unsupported.
            if data.state == protocol::ConnectionState::SUCCESSFUL
                || data.version.version >= Version::Win8
            {
                response.version_supported = 1;
                response.connection_state = data.state;
                response.selected_version_or_connection_id =
                    if data.version.version >= Version::Win10Rs3_1 {
                        self.inner.child_connection_id
                    } else {
                        data.version.version as u32
                    };

                if data.version.version >= Version::Copper {
                    response_copper.supported_features = data.version.feature_flags.into();
                    response_type = VersionResponseType::Copper;
                    if let Some(monitor_page) = data.monitor_pages {
                        assert!(data.version.feature_flags.server_specified_monitor_pages());
                        response_copper_with_mnf.child_to_parent_monitor_page_gpa =
                            monitor_page.child_to_parent;
                        response_copper_with_mnf.parent_to_child_monitor_page_gpa =
                            monitor_page.parent_to_child;
                        response_type = VersionResponseType::CopperWithServerMnf;
                    }
                }
            }
        }

        // Send the correct type of response based on the negotiated version and flags.
        match response_type {
            VersionResponseType::PreCopper => {
                self.sender().send_message_with_target(response, target)
            }
            VersionResponseType::Copper => self
                .sender()
                .send_message_with_target(response_copper, target),
            VersionResponseType::CopperWithServerMnf => self
                .sender()
                .send_message_with_target(&response_copper_with_mnf, target),
        }
    }

    /// Disconnects the guest, putting the server into `new_state` and returning
    /// false if there are channels that are not yet fully reset.
    fn request_disconnect(&mut self, new_action: ConnectionAction) -> bool {
        assert!(!self.is_resetting());

        // Release all channels.
        let gpadls = &mut self.inner.gpadls;
        let vm_reset = matches!(new_action, ConnectionAction::Reset);
        self.inner.channels.retain(|offer_id, channel| {
            // Release reserved channels only if the VM is resetting
            (!vm_reset && channel.state.is_reserved())
                || !Self::client_release_channel(
                    self.inner
                        .pending_messages
                        .sender(self.notifier, self.inner.state.is_paused()),
                    offer_id,
                    channel,
                    gpadls,
                    &mut self.inner.incomplete_gpadls,
                    &mut self.inner.assigned_channels,
                    &mut self.inner.assigned_monitors,
                    None,
                )
        });

        // Transition to disconnected or one of the pending disconnect states,
        // depending on whether there are still GPADLs or channels in use by the
        // server.
        match &mut self.inner.state {
            ConnectionState::Disconnected => {
                // Cleanup open reserved channels when doing disconnected VM reset
                if vm_reset {
                    if !self.are_channels_reset(true) {
                        self.inner.state = ConnectionState::Disconnecting {
                            next_action: ConnectionAction::Reset,
                            modify_sent: false,
                        };
                    }
                } else {
                    assert!(self.are_channels_reset(false));
                }
            }

            ConnectionState::Connected { .. } => {
                if self.are_channels_reset(vm_reset) {
                    self.notify_disconnect(new_action);
                } else {
                    self.inner.state = ConnectionState::Disconnecting {
                        next_action: new_action,
                        modify_sent: false,
                    };
                }
            }

            ConnectionState::Connecting { next_action, .. }
            | ConnectionState::Disconnecting { next_action, .. } => {
                *next_action = new_action;
            }
        }

        matches!(self.inner.state, ConnectionState::Disconnected)
    }

    pub(crate) fn complete_disconnect(&mut self) {
        if let ConnectionState::Disconnecting {
            next_action,
            modify_sent,
        } = std::mem::replace(&mut self.inner.state, ConnectionState::Disconnected)
        {
            assert!(self.are_channels_reset(matches!(next_action, ConnectionAction::Reset)));
            if !modify_sent {
                tracelimit::warn_ratelimited!("unexpected modify response");
            }

            self.inner.state = ConnectionState::Disconnected;
            self.do_next_action(next_action);
        } else {
            unreachable!("not ready for disconnect");
        }
    }

    fn do_next_action(&mut self, action: ConnectionAction) {
        match action {
            ConnectionAction::None => {}
            ConnectionAction::Reset => {
                self.complete_reset();
            }
            ConnectionAction::SendUnloadComplete => {
                self.complete_unload();
            }
            ConnectionAction::Reconnect { initiate_contact } => {
                self.initiate_contact(initiate_contact);
            }
            ConnectionAction::SendFailedVersionResponse => {
                // Used when the relay didn't support the requested version, so send a failed
                // response.
                self.send_version_response(None);
            }
        }
    }

    /// Handles MessageType::UNLOAD, which disconnects the guest.
    fn handle_unload(&mut self) {
        tracing::debug!(
            vtl = self.inner.assigned_channels.vtl as u8,
            state = ?self.inner.state,
            "VmBus received unload request from guest",
        );

        if self.request_disconnect(ConnectionAction::SendUnloadComplete) {
            self.complete_unload();
        }
    }

    fn complete_unload(&mut self) {
        self.notifier.unload_complete();
        if let Some(version) = self.inner.delayed_max_version.take() {
            self.inner.set_compatibility_version(version, false);
        }

        self.sender().send_message(&protocol::UnloadComplete {});
        tracelimit::info_ratelimited!("Vmbus disconnected");
    }

    /// Handles MessageType::REQUEST_OFFERS, which requests a list of channel offers.
    fn handle_request_offers(&mut self) -> Result<(), ChannelError> {
        let ConnectionState::Connected(info) = &mut self.inner.state else {
            unreachable!(
                "in unexpected state {:?}, should be prevented by Message::parse()",
                self.inner.state
            );
        };

        if info.offers_sent {
            return Err(ChannelError::OffersAlreadySent);
        }

        info.offers_sent = true;

        // Some guests expects channel IDs to stay consistent across hibernation and resume, so sort
        // the current offers before assigning channel IDs.
        let mut sorted_channels: Vec<_> = self
            .inner
            .channels
            .iter_mut()
            .filter(|(_, channel)| !channel.state.is_reserved())
            .collect();

        if self.inner.use_absolute_channel_order {
            sorted_channels.sort_unstable_by_key(|(_, channel)| {
                (
                    channel.offer.offer_order.unwrap_or(u64::MAX),
                    channel.offer.interface_id,
                    channel.offer.instance_id,
                )
            });
        } else {
            sorted_channels.sort_unstable_by_key(|(_, channel)| {
                (
                    channel.offer.interface_id,
                    channel.offer.offer_order.unwrap_or(u64::MAX),
                    channel.offer.instance_id,
                )
            });
        }

        for (offer_id, channel) in sorted_channels {
            assert!(matches!(channel.state, ChannelState::ClientReleased));

            channel.prepare_channel(
                offer_id,
                &mut self.inner.assigned_channels,
                &mut self.inner.assigned_monitors,
            );

            channel.state = ChannelState::Closed;
            self.inner
                .pending_messages
                .sender(self.notifier, info.paused)
                .send_offer(channel, info);
        }
        self.sender().send_message(&protocol::AllOffersDelivered {});

        Ok(())
    }

    /// Sends a GPADL to the device when `ranges` is Some. Returns false if the
    /// GPADL should be removed because the channel is already revoked.
    #[must_use]
    fn gpadl_updated(
        mut sender: MessageSender<'_, N>,
        offer_id: OfferId,
        channel: &Channel,
        gpadl_id: GpadlId,
        gpadl: &Gpadl,
    ) -> bool {
        if channel.state.is_revoked() {
            let channel_id = channel.info.as_ref().expect("assigned").channel_id;
            sender.send_gpadl_created(channel_id, gpadl_id, protocol::STATUS_UNSUCCESSFUL);
            false
        } else {
            // Notify the channel if the GPADL is done.
            sender.notifier.notify(
                offer_id,
                Action::Gpadl(gpadl_id, gpadl.count, gpadl.buf.clone()),
            );
            true
        }
    }

    /// Handles MessageType::GPADL_HEADER, which creates a new GPADL.
    fn handle_gpadl_header_core(
        &mut self,
        input: &protocol::GpadlHeader,
        range: &[u8],
    ) -> Result<(), ChannelError> {
        // Validate the channel ID.
        let (offer_id, channel) = self
            .inner
            .channels
            .get_by_channel_id_mut(&self.inner.assigned_channels, input.channel_id)?;

        // GPADL body messages don't contain the channel ID, so prevent creating new
        // GPADLs for reserved channels to avoid GPADL ID conflicts.
        if channel.state.is_reserved() {
            return Err(ChannelError::ChannelReserved);
        }

        // Create a new GPADL.
        let mut gpadl = Gpadl::new(input.count, input.len as usize / 8);
        let done = gpadl.append(range)?;

        // Store the GPADL in the table.
        let gpadl = match self.inner.gpadls.entry((input.gpadl_id, offer_id)) {
            Entry::Vacant(entry) => entry.insert(gpadl),
            Entry::Occupied(_) => return Err(ChannelError::DuplicateGpadlId),
        };

        // If we're not done, track the offer ID for GPADL body requests
        // N.B. The above only checks if the combination of (gpadl_id, offer_id) is unique, which
        //      allows for a guest to reuse a gpadl ID in use by a reserved channel (which it may
        //      not know about). But for in-progress GPADLs we need to ensure the gpadl ID itself
        //      is unique, since the body message doesn't include a channel ID.
        if !done {
            match self.inner.incomplete_gpadls.entry(input.gpadl_id) {
                Entry::Vacant(entry) => {
                    entry.insert(offer_id);
                }
                Entry::Occupied(_) => {
                    self.inner.gpadls.remove(&(input.gpadl_id, offer_id));
                    tracelimit::error_ratelimited!(
                        channel_id = ?input.channel_id,
                        key = %channel.offer.key(),
                        gpadl_id = ?input.gpadl_id,
                        "duplicate in-progress gpadl ID",
                    );
                    return Err(ChannelError::DuplicateGpadlId);
                }
            }
        }

        if done
            && !Self::gpadl_updated(
                self.inner
                    .pending_messages
                    .sender(self.notifier, self.inner.state.is_paused()),
                offer_id,
                channel,
                input.gpadl_id,
                gpadl,
            )
        {
            self.inner.gpadls.remove(&(input.gpadl_id, offer_id));
        }
        Ok(())
    }

    /// Handles MessageType::GPADL_HEADER, which creates a new GPADL.
    fn handle_gpadl_header(&mut self, input: &protocol::GpadlHeader, range: &[u8]) {
        if let Err(err) = self.handle_gpadl_header_core(input, range) {
            tracelimit::warn_ratelimited!(
                err = &err as &dyn std::error::Error,
                channel_id = ?input.channel_id,
                key = %self.inner.channels.get_by_channel_id(&self.inner.assigned_channels, input.channel_id).map(|(_, c)| c.offer.key()).unwrap_or_default(),
                gpadl_id = ?input.gpadl_id,
                "error handling gpadl header"
            );

            // Inform the guest of any error during the header message.
            self.sender().send_gpadl_created(
                input.channel_id,
                input.gpadl_id,
                protocol::STATUS_UNSUCCESSFUL,
            );
        }
    }

    /// Handles MessageType::GPADL_BODY, which adds more to an in-progress
    /// GPADL.
    ///
    /// N.B. This function only returns an error if the error was not handled locally by sending an
    ///      error response to the guest.
    fn handle_gpadl_body(
        &mut self,
        input: &protocol::GpadlBody,
        range: &[u8],
    ) -> Result<(), ChannelError> {
        // Find and update the GPADL.
        // N.B. No error response can be sent to the guest if the gpadl ID is invalid, because the
        //      channel ID is not known in that case.
        let &offer_id = self
            .inner
            .incomplete_gpadls
            .get(&input.gpadl_id)
            .ok_or(ChannelError::UnknownGpadlId)?;
        let gpadl = self
            .inner
            .gpadls
            .get_mut(&(input.gpadl_id, offer_id))
            .ok_or(ChannelError::UnknownGpadlId)?;
        let channel = &mut self.inner.channels[offer_id];

        match gpadl.append(range) {
            Ok(done) => {
                if done {
                    self.inner.incomplete_gpadls.remove(&input.gpadl_id);
                    if !Self::gpadl_updated(
                        self.inner
                            .pending_messages
                            .sender(self.notifier, self.inner.state.is_paused()),
                        offer_id,
                        channel,
                        input.gpadl_id,
                        gpadl,
                    ) {
                        self.inner.gpadls.remove(&(input.gpadl_id, offer_id));
                    }
                }
            }
            Err(err) => {
                self.inner.incomplete_gpadls.remove(&input.gpadl_id);
                self.inner.gpadls.remove(&(input.gpadl_id, offer_id));
                let channel_id = channel.info.as_ref().expect("assigned").channel_id;
                tracelimit::warn_ratelimited!(
                    err = &err as &dyn std::error::Error,
                    channel_id = channel_id.0,
                    key = %channel.offer.key(),
                    gpadl_id = input.gpadl_id.0,
                    "error handling gpadl body"
                );
                self.sender().send_gpadl_created(
                    channel_id,
                    input.gpadl_id,
                    protocol::STATUS_UNSUCCESSFUL,
                );
            }
        }

        Ok(())
    }

    /// Handles MessageType::GPADL_TEARDOWN, which tears down a GPADL.
    fn handle_gpadl_teardown(
        &mut self,
        input: &protocol::GpadlTeardown,
    ) -> Result<(), ChannelError> {
        let (offer_id, channel) = self
            .inner
            .channels
            .get_by_channel_id_mut(&self.inner.assigned_channels, input.channel_id)?;

        tracing::debug!(
            channel_id = input.channel_id.0,
            key = %channel.offer.key(),
            gpadl_id = input.gpadl_id.0,
            "Received GPADL teardown request"
        );

        let gpadl = self
            .inner
            .gpadls
            .get_mut(&(input.gpadl_id, offer_id))
            .ok_or(ChannelError::UnknownGpadlId)?;

        match gpadl.state {
            GpadlState::InProgress
            | GpadlState::Offered
            | GpadlState::OfferedTearingDown
            | GpadlState::TearingDown => {
                return Err(ChannelError::InvalidGpadlState);
            }
            GpadlState::Accepted => {
                if channel.info.as_ref().map(|info| info.channel_id) != Some(input.channel_id) {
                    return Err(ChannelError::WrongGpadlChannelId);
                }

                // GPADL IDs must be unique during teardown. Disallow reserved
                // channels to avoid collisions with non-reserved channel GPADL
                // IDs across disconnects.
                if channel.state.is_reserved() {
                    return Err(ChannelError::ChannelReserved);
                }

                if channel.state.is_revoked() {
                    tracing::trace!(
                        channel_id = input.channel_id.0,
                        key = %channel.offer.key(),
                        gpadl_id = input.gpadl_id.0,
                        "Gpadl teardown for revoked channel"
                    );

                    self.inner.gpadls.remove(&(input.gpadl_id, offer_id));
                    self.sender().send_gpadl_torndown(input.gpadl_id);
                } else {
                    gpadl.state = GpadlState::TearingDown;
                    self.notifier.notify(
                        offer_id,
                        Action::TeardownGpadl {
                            gpadl_id: input.gpadl_id,
                            post_restore: false,
                        },
                    );
                }
            }
        }
        Ok(())
    }

    /// Moves a channel from the `Closed` to `Opening` state, notifying the
    /// device.
    fn open_channel(
        &mut self,
        offer_id: OfferId,
        input: &OpenRequest,
        reserved_state: Option<ReservedState>,
    ) {
        let channel = &mut self.inner.channels[offer_id];
        assert!(matches!(channel.state, ChannelState::Closed));

        channel.state = ChannelState::Opening {
            request: *input,
            reserved_state,
        };

        // Do not update info with the guest-provided connection ID, since the
        // value must be remembered if the channel is closed and re-opened.
        let info = channel.info.as_ref().expect("assigned");
        self.notifier.notify(
            offer_id,
            Action::Open(
                OpenParams::from_request(
                    info,
                    input,
                    channel.handled_monitor_info(),
                    reserved_state.map(|state| state.target),
                ),
                self.inner.state.get_version().expect("must be connected"),
            ),
        );
    }

    /// Handles MessageType::OPEN_CHANNEL, which opens a channel.
    fn handle_open_channel(&mut self, input: &protocol::OpenChannel2) -> Result<(), ChannelError> {
        let (offer_id, channel) = self
            .inner
            .channels
            .get_by_channel_id_mut(&self.inner.assigned_channels, input.open_channel.channel_id)?;

        let guest_specified_interrupt_info = self
            .inner
            .state
            .check_feature_flags(|ff| ff.guest_specified_signal_parameters())
            .then_some(SignalInfo {
                event_flag: input.event_flag,
                connection_id: input.connection_id,
            });

        let flags = if self
            .inner
            .state
            .check_feature_flags(|ff| ff.channel_interrupt_redirection())
        {
            input.flags
        } else {
            Default::default()
        };

        let request = OpenRequest {
            open_id: input.open_channel.open_id,
            ring_buffer_gpadl_id: input.open_channel.ring_buffer_gpadl_id,
            target_vp: input.open_channel.target_vp,
            downstream_ring_buffer_page_offset: input
                .open_channel
                .downstream_ring_buffer_page_offset,
            user_data: input.open_channel.user_data,
            guest_specified_interrupt_info,
            flags,
        };

        match channel.state {
            ChannelState::Closed => self.open_channel(offer_id, &request, None),
            ChannelState::Closing { params, .. } => {
                // Since there is no close complete message, this can happen
                // after the ring buffer GPADL is released but before the server
                // completes the close request.
                channel.state = ChannelState::ClosingReopen { params, request }
            }
            ChannelState::Revoked | ChannelState::Reoffered => {}

            ChannelState::Open { .. }
            | ChannelState::Opening { .. }
            | ChannelState::ClosingReopen { .. } => return Err(ChannelError::ChannelAlreadyOpen),

            ChannelState::ClientReleased
            | ChannelState::ClosingClientRelease
            | ChannelState::OpeningClientRelease => unreachable!(),
        }
        Ok(())
    }

    /// Handles MessageType::CLOSE_CHANNEL, which closes a channel.
    fn handle_close_channel(&mut self, input: &protocol::CloseChannel) -> Result<(), ChannelError> {
        let (offer_id, channel) = self
            .inner
            .channels
            .get_by_channel_id_mut(&self.inner.assigned_channels, input.channel_id)?;

        match channel.state {
            ChannelState::Open {
                params,
                modify_state,
                reserved_state: None,
            } => {
                if modify_state.is_modifying() {
                    tracelimit::warn_ratelimited!(
                        key = %channel.offer.key(),
                        ?modify_state,
                        "Client is closing the channel with a modify in progress"
                    )
                }

                channel.state = ChannelState::Closing {
                    params,
                    reserved_state: None,
                };
                self.notifier.notify(offer_id, Action::Close);
            }

            ChannelState::Open {
                reserved_state: Some(_),
                ..
            } => return Err(ChannelError::ChannelReserved),

            ChannelState::Revoked | ChannelState::Reoffered => {}

            ChannelState::Closed
            | ChannelState::Opening { .. }
            | ChannelState::Closing { .. }
            | ChannelState::ClosingReopen { .. } => return Err(ChannelError::ChannelNotOpen),

            ChannelState::ClientReleased
            | ChannelState::ClosingClientRelease
            | ChannelState::OpeningClientRelease => unreachable!(),
        }

        Ok(())
    }

    /// Handles MessageType::OPEN_RESERVED_CHANNEL, which reserves and opens a channel.
    /// The version must have already been validated in parse_message.
    fn handle_open_reserved_channel(
        &mut self,
        input: &protocol::OpenReservedChannel,
        version: VersionInfo,
    ) -> Result<(), ChannelError> {
        let (offer_id, channel) = self
            .inner
            .channels
            .get_by_channel_id_mut(&self.inner.assigned_channels, input.channel_id)?;

        let target = ConnectionTarget {
            vp: input.target_vp,
            sint: input.target_sint as u8,
        };

        let reserved_state = Some(ReservedState { version, target });

        let request = OpenRequest {
            ring_buffer_gpadl_id: input.ring_buffer_gpadl,
            // Interrupts are disabled for reserved channels; this matches Hyper-V behavior.
            target_vp: protocol::VP_INDEX_DISABLE_INTERRUPT,
            downstream_ring_buffer_page_offset: input.downstream_page_offset,
            open_id: 0,
            user_data: UserDefinedData::new_zeroed(),
            guest_specified_interrupt_info: None,
            flags: Default::default(),
        };

        match channel.state {
            ChannelState::Closed => self.open_channel(offer_id, &request, reserved_state),
            ChannelState::Revoked | ChannelState::Reoffered => {}

            ChannelState::Open { .. } | ChannelState::Opening { .. } => {
                return Err(ChannelError::ChannelAlreadyOpen);
            }

            ChannelState::Closing { .. } | ChannelState::ClosingReopen { .. } => {
                return Err(ChannelError::InvalidChannelState);
            }

            ChannelState::ClientReleased
            | ChannelState::ClosingClientRelease
            | ChannelState::OpeningClientRelease => unreachable!(),
        }
        Ok(())
    }

    /// Handles MessageType::CLOSE_RESERVED_CHANNEL, which closes a reserved channel. Will send
    /// the response to the target provided in the request instead of the current reserved target.
    fn handle_close_reserved_channel(
        &mut self,
        input: &protocol::CloseReservedChannel,
    ) -> Result<(), ChannelError> {
        let (offer_id, channel) = self
            .inner
            .channels
            .get_by_channel_id_mut(&self.inner.assigned_channels, input.channel_id)?;

        match channel.state {
            ChannelState::Open {
                params,
                reserved_state: Some(mut resvd),
                ..
            } => {
                resvd.target.vp = input.target_vp;
                resvd.target.sint = input.target_sint as u8;
                channel.state = ChannelState::Closing {
                    params,
                    reserved_state: Some(resvd),
                };
                self.notifier.notify(offer_id, Action::Close);
            }

            ChannelState::Open {
                reserved_state: None,
                ..
            } => return Err(ChannelError::ChannelNotReserved),

            ChannelState::Revoked | ChannelState::Reoffered => {}

            ChannelState::Closed
            | ChannelState::Opening { .. }
            | ChannelState::Closing { .. }
            | ChannelState::ClosingReopen { .. } => return Err(ChannelError::ChannelNotOpen),

            ChannelState::ClientReleased
            | ChannelState::ClosingClientRelease
            | ChannelState::OpeningClientRelease => unreachable!(),
        }

        Ok(())
    }

    /// Release all guest references on a channel, including GPADLs that are
    /// associated with the channel. Returns true if the channel should be
    /// deleted.
    #[must_use]
    fn client_release_channel(
        mut sender: MessageSender<'_, N>,
        offer_id: OfferId,
        channel: &mut Channel,
        gpadls: &mut GpadlMap,
        incomplete_gpadls: &mut IncompleteGpadlMap,
        assigned_channels: &mut AssignedChannels,
        assigned_monitors: &mut AssignedMonitors,
        info: Option<&ConnectionInfo>,
    ) -> bool {
        tracelimit::info_ratelimited!(?offer_id, key = %channel.offer.key(), "client released channel");
        // Release any GPADLs that remain for this channel.
        gpadls.retain(|&(gpadl_id, gpadl_offer_id), gpadl| {
            if gpadl_offer_id != offer_id {
                return true;
            }
            match gpadl.state {
                GpadlState::InProgress => {
                    incomplete_gpadls.remove(&gpadl_id);
                    false
                }
                GpadlState::Offered => {
                    gpadl.state = GpadlState::OfferedTearingDown;
                    true
                }
                GpadlState::Accepted => {
                    if channel.state.is_revoked() {
                        // There is no need to tear down the GPADL.
                        false
                    } else {
                        gpadl.state = GpadlState::TearingDown;
                        sender.notifier.notify(
                            offer_id,
                            Action::TeardownGpadl {
                                gpadl_id,
                                post_restore: false,
                            },
                        );
                        true
                    }
                }
                GpadlState::OfferedTearingDown | GpadlState::TearingDown => true,
            }
        });

        let remove = match &mut channel.state {
            ChannelState::Closed => {
                channel.state = ChannelState::ClientReleased;
                false
            }
            ChannelState::Reoffered => {
                if let Some(info) = info {
                    channel.state = ChannelState::Closed;
                    channel.restore_state = RestoreState::New;
                    sender.send_offer(channel, info);
                    // Do not release the channel ID.
                    return false;
                }
                channel.state = ChannelState::ClientReleased;
                false
            }
            ChannelState::Revoked => {
                channel.state = ChannelState::ClientReleased;
                true
            }
            ChannelState::Opening { .. } => {
                channel.state = ChannelState::OpeningClientRelease;
                false
            }
            ChannelState::Open { .. } => {
                channel.state = ChannelState::ClosingClientRelease;
                sender.notifier.notify(offer_id, Action::Close);
                false
            }
            ChannelState::Closing { .. } | ChannelState::ClosingReopen { .. } => {
                channel.state = ChannelState::ClosingClientRelease;
                false
            }

            ChannelState::ClosingClientRelease
            | ChannelState::OpeningClientRelease
            | ChannelState::ClientReleased => false,
        };

        assert!(channel.state.is_released());

        channel.release_channel(offer_id, assigned_channels, assigned_monitors);
        remove
    }

    /// Handles MessageType::REL_ID_RELEASED, which releases the guest references to a channel.
    fn handle_rel_id_released(
        &mut self,
        input: &protocol::RelIdReleased,
    ) -> Result<(), ChannelError> {
        let channel_id = input.channel_id;
        let (offer_id, channel) = self
            .inner
            .channels
            .get_by_channel_id_mut(&self.inner.assigned_channels, channel_id)?;

        match channel.state {
            ChannelState::Closed
            | ChannelState::Revoked
            | ChannelState::Closing { .. }
            | ChannelState::Reoffered => {
                if Self::client_release_channel(
                    self.inner
                        .pending_messages
                        .sender(self.notifier, self.inner.state.is_paused()),
                    offer_id,
                    channel,
                    &mut self.inner.gpadls,
                    &mut self.inner.incomplete_gpadls,
                    &mut self.inner.assigned_channels,
                    &mut self.inner.assigned_monitors,
                    self.inner.state.get_connected_info(),
                ) {
                    self.inner.channels.remove(offer_id);
                }

                self.check_disconnected();
            }

            ChannelState::Opening { .. }
            | ChannelState::Open { .. }
            | ChannelState::ClosingReopen { .. } => return Err(ChannelError::InvalidChannelState),

            ChannelState::ClientReleased
            | ChannelState::OpeningClientRelease
            | ChannelState::ClosingClientRelease => unreachable!(),
        }
        Ok(())
    }

    /// Handles MessageType::TL_CONNECT_REQUEST, which requests for an hvsocket
    /// connection.
    fn handle_tl_connect_request(&mut self, request: protocol::TlConnectRequest2) {
        let version = self
            .inner
            .state
            .get_version()
            .expect("must be connected")
            .version;

        let hosted_silo_unaware = version < Version::Win10Rs5;
        self.notifier
            .notify_hvsock(&HvsockConnectRequest::from_message(
                request,
                hosted_silo_unaware,
            ));
    }

    /// Sends a message to the guest if an hvsocket connect request failed.
    pub fn send_tl_connect_result(&mut self, result: HvsockConnectResult) {
        // TODO: need save/restore handling for this... probably OK to just drop
        // all such requests given hvsock's general lack of save/restore
        // support.
        if !result.success && self.inner.state.check_version(Version::Win10Rs3_0) {
            // Windows guests care about the error code used here; using STATUS_CONNECTION_REFUSED
            // ensures a sensible error gets returned to the user that tried to connect to the
            // socket.
            self.sender().send_message(&protocol::TlConnectResult {
                service_id: result.service_id,
                endpoint_id: result.endpoint_id,
                status: protocol::STATUS_CONNECTION_REFUSED,
            })
        }
    }

    /// Handles MessageType::MODIFY_CHANNEL, which allows the guest to request a
    /// new target VP for the channel's interrupts.
    fn handle_modify_channel(
        &mut self,
        request: &protocol::ModifyChannel,
    ) -> Result<(), ChannelError> {
        let result = self.modify_channel(request);
        if result.is_err() {
            self.send_modify_channel_response(request.channel_id, protocol::STATUS_UNSUCCESSFUL);
        }

        result
    }

    /// Modifies a channel's target VP.
    fn modify_channel(&mut self, request: &protocol::ModifyChannel) -> Result<(), ChannelError> {
        let (offer_id, channel) = self
            .inner
            .channels
            .get_by_channel_id_mut(&self.inner.assigned_channels, request.channel_id)?;

        let (open_request, modify_state) = match &mut channel.state {
            ChannelState::Open {
                params,
                modify_state,
                reserved_state: None,
            } => (params, modify_state),
            _ => return Err(ChannelError::InvalidChannelState),
        };

        if let ModifyState::Modifying { pending_target_vp } = modify_state {
            if self.inner.state.check_version(Version::Iron) {
                // On Iron or later, the client isn't allowed to send a ModifyChannel
                // request while another one is still in progress.
                tracelimit::warn_ratelimited!(
                    key = %channel.offer.key(),
                    "Client sent new ModifyChannel before receiving ModifyChannelResponse."
                );
            } else {
                // On older versions, the client doesn't know if the operation is complete,
                // so store the latest request to execute when the current one completes.
                *pending_target_vp = Some(request.target_vp);
            }
        } else {
            self.notifier.notify(
                offer_id,
                Action::Modify {
                    target_vp: request.target_vp,
                },
            );

            // Update the stored open_request so that save/restore will use the new value.
            open_request.target_vp = request.target_vp;
            *modify_state = ModifyState::Modifying {
                pending_target_vp: None,
            };
        }

        Ok(())
    }

    /// Complete the ModifyChannel message.
    ///
    /// N.B. The guest expects no further interrupts on the old VP at this point. This
    ///      is guaranteed because notify() handles updating the event port synchronously before,
    ///      notifying the device/relay, and all types of event port protect their VP settings
    ///      with locks.
    pub fn modify_channel_complete(&mut self, offer_id: OfferId, status: i32) {
        let channel = &mut self.inner.channels[offer_id];

        if let ChannelState::Open {
            params,
            modify_state: ModifyState::Modifying { pending_target_vp },
            reserved_state: None,
        } = channel.state
        {
            channel.state = ChannelState::Open {
                params,
                modify_state: ModifyState::NotModifying,
                reserved_state: None,
            };

            // Send the ModifyChannelResponse message if the protocol supports it.
            let channel_id = channel.info.as_ref().expect("assigned").channel_id;
            let key = channel.offer.key();
            self.send_modify_channel_response(channel_id, status);

            // Handle a pending ModifyChannel request if there is one.
            if let Some(target_vp) = pending_target_vp {
                let request = protocol::ModifyChannel {
                    channel_id,
                    target_vp,
                };

                if let Err(error) = self.handle_modify_channel(&request) {
                    tracelimit::warn_ratelimited!(?error, %key, "Pending ModifyChannel request failed.")
                }
            }
        }
    }

    fn send_modify_channel_response(&mut self, channel_id: ChannelId, status: i32) {
        if self.inner.state.check_version(Version::Iron) {
            self.sender()
                .send_message(&protocol::ModifyChannelResponse { channel_id, status });
        }
    }

    fn handle_modify_connection(&mut self, request: protocol::ModifyConnection) {
        if let Err(err) = self.modify_connection(request) {
            tracelimit::error_ratelimited!(?err, "modifying connection failed");
            self.complete_modify_connection(ModifyConnectionResponse::Modified(
                protocol::ConnectionState::FAILED_UNKNOWN_FAILURE,
            ));
        }
    }

    fn modify_connection(&mut self, request: protocol::ModifyConnection) -> anyhow::Result<()> {
        let ConnectionState::Connected(info) = &mut self.inner.state else {
            anyhow::bail!(
                "Invalid state for ModifyConnection request: {:?}",
                self.inner.state
            );
        };

        if info.modifying {
            anyhow::bail!(
                "Duplicate ModifyConnection request, state: {:?}",
                self.inner.state
            );
        }

        if matches!(
            info.monitor_page,
            Some(MonitorPageGpaInfo {
                server_allocated: true,
                ..
            })
        ) {
            anyhow::bail!("Cannot modify server-allocated monitor pages");
        }

        if (request.child_to_parent_monitor_page_gpa == 0)
            != (request.parent_to_child_monitor_page_gpa == 0)
        {
            anyhow::bail!("Guest must specify either both or no monitor pages, {request:?}");
        }

        let monitor_page = (request.child_to_parent_monitor_page_gpa != 0).then_some(
            MonitorPageGpaInfo::from_guest_gpas(MonitorPageGpas {
                child_to_parent: request.child_to_parent_monitor_page_gpa,
                parent_to_child: request.parent_to_child_monitor_page_gpa,
            }),
        );

        info.modifying = true;
        info.monitor_page = monitor_page;
        tracing::debug!("modifying connection parameters.");
        self.notifier.modify_connection(request.into())?;

        Ok(())
    }

    pub fn complete_modify_connection(&mut self, response: ModifyConnectionResponse) {
        tracing::debug!(?response, "modifying connection parameters complete");

        // InitiateContact, Unload, and actual ModifyConnection messages are all sent to the relay
        // as ModifyConnection requests, so use the server state to determine how to handle the
        // response.
        match &mut self.inner.state {
            ConnectionState::Connecting { .. } => self.complete_initiate_contact(response),
            ConnectionState::Disconnecting { .. } => self.complete_disconnect(),
            ConnectionState::Connected(info) => {
                let ModifyConnectionResponse::Modified(connection_state) = response else {
                    panic!(
                        "Relay should not return {:?} for a modify request with no version.",
                        response
                    );
                };

                if !info.modifying {
                    panic!(
                        "ModifyConnection response while not modifying, state: {:?}",
                        self.inner.state
                    );
                }

                info.modifying = false;
                self.sender()
                    .send_message(&protocol::ModifyConnectionResponse { connection_state });
            }
            _ => panic!(
                "Invalid state for ModifyConnection response: {:?}",
                self.inner.state
            ),
        }
    }

    fn handle_pause(&mut self) {
        tracelimit::info_ratelimited!("pausing sending messages");
        self.sender().send_message(&protocol::PauseResponse {});
        let ConnectionState::Connected(info) = &mut self.inner.state else {
            unreachable!(
                "in unexpected state {:?}, should be prevented by Message::parse()",
                self.inner.state
            );
        };
        info.paused = true;
    }

    /// Processes an incoming message from the guest.
    pub fn handle_synic_message(&mut self, message: SynicMessage) -> Result<(), ChannelError> {
        assert!(!self.is_resetting());

        let version = self.inner.state.get_version();
        let msg = Message::parse(&message.data, version)?;
        tracing::trace!(?msg, message.trusted, "received vmbus message");
        // Do not allow untrusted messages if the connection was established
        // using a trusted message.
        //
        // TODO: Don't allow trusted messages if an untrusted connection was ever used.
        if self.inner.state.is_trusted() && !message.trusted {
            tracelimit::warn_ratelimited!(?msg, "Received untrusted message");
            return Err(ChannelError::UntrustedMessage);
        }

        // Unpause channel responses if they are paused.
        match &mut self.inner.state {
            ConnectionState::Connected(info) if info.paused => {
                if !matches!(
                    msg,
                    Message::Resume(..)
                        | Message::Unload(..)
                        | Message::InitiateContact { .. }
                        | Message::InitiateContact2 { .. }
                ) {
                    tracelimit::warn_ratelimited!(?msg, "Received message while paused");
                    return Err(ChannelError::Paused);
                }
                tracelimit::info_ratelimited!("resuming sending messages");
                info.paused = false;
            }
            _ => {}
        }

        match msg {
            Message::InitiateContact2(input, ..) => {
                self.handle_initiate_contact(&input, &message, true)?
            }
            Message::InitiateContact(input, ..) => {
                self.handle_initiate_contact(&input.into(), &message, false)?
            }
            Message::Unload(..) => self.handle_unload(),
            Message::RequestOffers(..) => self.handle_request_offers()?,
            Message::GpadlHeader(input, range) => self.handle_gpadl_header(&input, range),
            Message::GpadlBody(input, range) => self.handle_gpadl_body(&input, range)?,
            Message::GpadlTeardown(input, ..) => self.handle_gpadl_teardown(&input)?,
            Message::OpenChannel(input, ..) => self.handle_open_channel(&input.into())?,
            Message::OpenChannel2(input, ..) => self.handle_open_channel(&input)?,
            Message::CloseChannel(input, ..) => self.handle_close_channel(&input)?,
            Message::RelIdReleased(input, ..) => self.handle_rel_id_released(&input)?,
            Message::TlConnectRequest(input, ..) => self.handle_tl_connect_request(input.into()),
            Message::TlConnectRequest2(input, ..) => self.handle_tl_connect_request(input),
            Message::ModifyChannel(input, ..) => self.handle_modify_channel(&input)?,
            Message::ModifyConnection(input, ..) => self.handle_modify_connection(input),
            Message::OpenReservedChannel(input, ..) => self.handle_open_reserved_channel(
                &input,
                version.expect("version validated by Message::parse"),
            )?,
            Message::CloseReservedChannel(input, ..) => {
                self.handle_close_reserved_channel(&input)?
            }
            Message::Pause(protocol::Pause, ..) => self.handle_pause(),
            Message::Resume(protocol::Resume, ..) => {}
            // Messages that should only be received by a vmbus client.
            Message::OfferChannel(..)
            | Message::RescindChannelOffer(..)
            | Message::AllOffersDelivered(..)
            | Message::OpenResult(..)
            | Message::GpadlCreated(..)
            | Message::GpadlTorndown(..)
            | Message::VersionResponse(..)
            | Message::VersionResponse2(..)
            | Message::VersionResponse3(..)
            | Message::UnloadComplete(..)
            | Message::CloseReservedChannelResponse(..)
            | Message::TlConnectResult(..)
            | Message::ModifyChannelResponse(..)
            | Message::ModifyConnectionResponse(..)
            | Message::PauseResponse(..) => {
                unreachable!("Server received client message {:?}", msg);
            }
        }
        Ok(())
    }

    /// Completes a GPADL creation, accepting it if `status >= 0`, rejecting it otherwise.
    pub fn gpadl_create_complete(&mut self, offer_id: OfferId, gpadl_id: GpadlId, status: i32) {
        let Some(gpadl) = self.inner.gpadls.get_mut(&(gpadl_id, offer_id)) else {
            tracelimit::error_ratelimited!(
                ?offer_id,
                key = %self.inner.channels[offer_id].offer.key(),
                ?gpadl_id,
                "invalid gpadl ID for channel"
            );
            return;
        };
        let retain = match gpadl.state {
            GpadlState::InProgress | GpadlState::TearingDown | GpadlState::Accepted => {
                tracelimit::error_ratelimited!(?offer_id, ?gpadl_id, ?gpadl, "invalid gpadl state");
                return;
            }
            GpadlState::Offered => {
                let channel_id = self.inner.channels[offer_id]
                    .info
                    .as_ref()
                    .expect("assigned")
                    .channel_id;
                self.inner
                    .pending_messages
                    .sender(self.notifier, self.inner.state.is_paused())
                    .send_gpadl_created(channel_id, gpadl_id, status);
                if status >= 0 {
                    gpadl.state = GpadlState::Accepted;
                    true
                } else {
                    false
                }
            }
            GpadlState::OfferedTearingDown => {
                if status >= 0 {
                    // Tear down the GPADL immediately.
                    self.notifier.notify(
                        offer_id,
                        Action::TeardownGpadl {
                            gpadl_id,
                            post_restore: false,
                        },
                    );
                    gpadl.state = GpadlState::TearingDown;
                    true
                } else {
                    false
                }
            }
        };
        if !retain {
            self.inner
                .gpadls
                .remove(&(gpadl_id, offer_id))
                .expect("gpadl validated above");

            self.check_disconnected();
        }
    }

    /// Releases a GPADL that is being torn down.
    pub fn gpadl_teardown_complete(&mut self, offer_id: OfferId, gpadl_id: GpadlId) {
        let channel = &mut self.inner.channels[offer_id];
        let Some(gpadl) = self.inner.gpadls.get_mut(&(gpadl_id, offer_id)) else {
            tracelimit::error_ratelimited!(
                ?offer_id,
                key = %channel.offer.key(),
                ?gpadl_id,
                "invalid gpadl ID for channel"
            );
            return;
        };
        tracing::debug!(
            offer_id = offer_id.0,
            key = %channel.offer.key(),
            gpadl_id = gpadl_id.0,
            "Gpadl teardown complete"
        );
        match gpadl.state {
            GpadlState::InProgress
            | GpadlState::Offered
            | GpadlState::OfferedTearingDown
            | GpadlState::Accepted => {
                tracelimit::error_ratelimited!(?offer_id, key = %channel.offer.key(), ?gpadl_id, ?gpadl, "invalid gpadl state");
            }
            GpadlState::TearingDown => {
                if !channel.state.is_released() {
                    self.sender().send_gpadl_torndown(gpadl_id);
                }
                self.inner
                    .gpadls
                    .remove(&(gpadl_id, offer_id))
                    .expect("gpadl validated above");

                self.check_disconnected();
            }
        }
    }

    /// Creates a sender, in a convenient way for callers that are able to borrow all of `self`.
    ///
    /// If you cannot borrow all of `self`, you will need to use the `PendingMessages::sender`
    /// method instead.
    fn sender(&mut self) -> MessageSender<'_, N> {
        self.inner
            .pending_messages
            .sender(self.notifier, self.inner.state.is_paused())
    }
}

fn revoke<N: Notifier>(
    mut sender: MessageSender<'_, N>,
    offer_id: OfferId,
    channel: &mut Channel,
    gpadls: &mut GpadlMap,
) -> bool {
    let info = match channel.state {
        ChannelState::Closed
        | ChannelState::Open { .. }
        | ChannelState::Opening { .. }
        | ChannelState::Closing { .. }
        | ChannelState::ClosingReopen { .. } => {
            channel.state = ChannelState::Revoked;
            Some(channel.info.as_ref().expect("assigned"))
        }
        ChannelState::Reoffered => {
            channel.state = ChannelState::Revoked;
            None
        }
        ChannelState::ClientReleased
        | ChannelState::OpeningClientRelease
        | ChannelState::ClosingClientRelease => None,
        // If the channel is being dropped, it may already have been revoked explicitly.
        ChannelState::Revoked => return true,
    };
    let retain = !channel.state.is_released();

    // Release any GPADLs.
    gpadls.retain(|&(gpadl_id, gpadl_offer_id), gpadl| {
        if gpadl_offer_id != offer_id {
            return true;
        }

        match gpadl.state {
            GpadlState::InProgress => true,
            GpadlState::Offered => {
                if let Some(info) = info {
                    sender.send_gpadl_created(
                        info.channel_id,
                        gpadl_id,
                        protocol::STATUS_UNSUCCESSFUL,
                    );
                }
                false
            }
            GpadlState::OfferedTearingDown => false,
            GpadlState::Accepted => true,
            GpadlState::TearingDown => {
                if info.is_some() {
                    sender.send_gpadl_torndown(gpadl_id);
                }
                false
            }
        }
    });
    if let Some(info) = info {
        sender.send_rescind(info);
    }
    // Revoking a channel effectively completes the restore operation for it.
    if channel.restore_state != RestoreState::New {
        channel.restore_state = RestoreState::Restored;
    }
    retain
}

struct PendingMessages(VecDeque<OutgoingMessage>);

impl PendingMessages {
    /// Creates a sender for the specified notifier.
    fn sender<'a, N: Notifier>(
        &'a mut self,
        notifier: &'a mut N,
        is_paused: bool,
    ) -> MessageSender<'a, N> {
        MessageSender {
            notifier,
            pending_messages: self,
            is_paused,
        }
    }
}

/// Wraps the state needed to send messages to the guest through the notifier, and queue them if
/// they are not immediately sent.
struct MessageSender<'a, N> {
    notifier: &'a mut N,
    pending_messages: &'a mut PendingMessages,
    is_paused: bool,
}

impl<N: Notifier> MessageSender<'_, N> {
    /// Sends a VMBus channel message to the guest.
    fn send_message<
        T: IntoBytes + protocol::VmbusMessage + std::fmt::Debug + Immutable + KnownLayout,
    >(
        &mut self,
        msg: &T,
    ) {
        let message = OutgoingMessage::new(msg);

        tracing::trace!(typ = ?T::MESSAGE_TYPE, ?msg, "sending message");
        // Don't try to send the message if there are already pending messages.
        if !self.pending_messages.0.is_empty()
            || self.is_paused
            || !self.notifier.send_message(&message, MessageTarget::Default)
        {
            tracing::trace!("message queued");
            // Queue the message for retry later.
            self.pending_messages.0.push_back(message);
        }
    }

    /// Sends a VMBus channel message to the guest via an alternate port.
    fn send_message_with_target<
        T: IntoBytes + protocol::VmbusMessage + std::fmt::Debug + Immutable + KnownLayout,
    >(
        &mut self,
        msg: &T,
        target: MessageTarget,
    ) {
        if target == MessageTarget::Default {
            self.send_message(msg);
        } else {
            tracing::trace!(typ = ?T::MESSAGE_TYPE, ?msg, "sending message");
            // Messages for other targets are not queued, nor are they affected
            // by the paused state.
            let message = OutgoingMessage::new(msg);
            if !self.notifier.send_message(&message, target) {
                tracelimit::warn_ratelimited!(?target, "failed to send message");
            }
        }
    }

    /// Sends a channel offer message to the guest.
    fn send_offer(&mut self, channel: &mut Channel, connection_info: &ConnectionInfo) {
        let info = channel.info.as_ref().expect("assigned");
        let mut flags = channel.offer.flags;
        if !connection_info
            .version
            .feature_flags
            .confidential_channels()
        {
            flags.set_confidential_ring_buffer(false);
            flags.set_confidential_external_memory(false);
        }

        // Send the monitor ID only if the guest supports MNF. MNF may also be disabled if the guest
        // provided monitor pages but this server can only use server-allocated monitor pages
        // (typically the case for OpenHCL on a hardware-isolated VM), but the guest didn't support
        // that. Since we cannot tell the guest to stop using MNF completely, sending the channel
        // without a monitor ID will prevent the guest from trying to use MNF to send interrupts for
        // it.
        let monitor_id = connection_info.monitor_page.and(info.monitor_id);
        let msg = protocol::OfferChannel {
            interface_id: channel.offer.interface_id,
            instance_id: channel.offer.instance_id,
            rsvd: [0; 4],
            flags,
            mmio_megabytes: channel.offer.mmio_megabytes,
            user_defined: channel.offer.user_defined,
            subchannel_index: channel.offer.subchannel_index,
            mmio_megabytes_optional: channel.offer.mmio_megabytes_optional,
            channel_id: info.channel_id,
            monitor_id: monitor_id.unwrap_or(MonitorId::INVALID).0,
            monitor_allocated: monitor_id.is_some().into(),
            // All channels are dedicated with Win8+ hosts.
            // These fields are sent to V1 guests as well, which will ignore them.
            is_dedicated: 1,
            connection_id: info.connection_id,
        };
        tracing::info!(
            channel_id = msg.channel_id.0,
            connection_id = msg.connection_id,
            key = %channel.offer.key(),
            "sending offer to guest"
        );

        self.send_message(&msg);
    }

    fn send_open_result(
        &mut self,
        channel_id: ChannelId,
        open_request: &OpenRequest,
        result: i32,
        target: MessageTarget,
    ) {
        self.send_message_with_target(
            &protocol::OpenResult {
                channel_id,
                open_id: open_request.open_id,
                status: result as u32,
            },
            target,
        );
    }

    fn send_gpadl_created(&mut self, channel_id: ChannelId, gpadl_id: GpadlId, status: i32) {
        self.send_message(&protocol::GpadlCreated {
            channel_id,
            gpadl_id,
            status,
        });
    }

    fn send_gpadl_torndown(&mut self, gpadl_id: GpadlId) {
        self.send_message(&protocol::GpadlTorndown { gpadl_id });
    }

    fn send_rescind(&mut self, info: &OfferedInfo) {
        tracing::info!(
            channel_id = info.channel_id.0,
            "rescinding channel from guest"
        );

        self.send_message(&protocol::RescindChannelOffer {
            channel_id: info.channel_id,
        });
    }
}

/// Provides information needed to send a VersionResponse message for a supported version.
struct VersionResponseData {
    version: VersionInfo,
    state: protocol::ConnectionState,
    monitor_pages: Option<MonitorPageGpas>,
}

impl VersionResponseData {
    /// Creates a new `VersionResponseData` with the negotiated version and connection state.
    fn new(version: VersionInfo, state: protocol::ConnectionState) -> Self {
        VersionResponseData {
            version,
            state,
            monitor_pages: None,
        }
    }

    /// Attaches server-allocated monitor pages to be sent with the response.
    fn with_monitor_pages(mut self, monitor_pages: Option<MonitorPageGpas>) -> Self {
        self.monitor_pages = monitor_pages;
        self
    }
}
