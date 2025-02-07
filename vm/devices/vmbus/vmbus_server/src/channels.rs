// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod saved_state;

use crate::monitor::AssignedMonitors;
use crate::protocol::Version;
use crate::Guid;
use crate::SynicMessage;
use crate::SINT;
use guestmem::GuestMemoryError;
use hvdef::Vtl;
use inspect::Inspect;
pub use saved_state::RestoreError;
pub use saved_state::SavedState;
use slab::Slab;
use std::cmp::min;
use std::collections::hash_map::Entry;
use std::collections::hash_map::HashMap;
use std::fmt::Display;
use std::ops::Index;
use std::ops::IndexMut;
use thiserror::Error;
use vmbus_channel::bus::ChannelType;
use vmbus_channel::bus::GpadlRequest;
use vmbus_channel::bus::OfferKey;
use vmbus_channel::bus::OfferParams;
use vmbus_channel::bus::OpenData;
use vmbus_channel::bus::RestoredGpadl;
use vmbus_core::protocol;
use vmbus_core::protocol::ChannelId;
use vmbus_core::protocol::ConnectionId;
use vmbus_core::protocol::FeatureFlags;
use vmbus_core::protocol::GpadlId;
use vmbus_core::protocol::Message;
use vmbus_core::protocol::OfferFlags;
use vmbus_core::protocol::UserDefinedData;
use vmbus_core::HvsockConnectRequest;
use vmbus_core::HvsockConnectResult;
use vmbus_core::MaxVersionInfo;
use vmbus_core::MonitorPageGpas;
use vmbus_core::OutgoingMessage;
use vmbus_core::VersionInfo;
use vmbus_ring::gparange;
use vmcore::monitor::MonitorId;
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
    #[error("an error occurred creating an event port")]
    SynicError(#[source] vmcore::synic::Error),
    #[error("an error occurred in the synic")]
    HypervisorError(#[source] vmcore::synic::HypervisorError),
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

        let mut trusted = false;
        if let Some(info) = info {
            resp.field(
                "protocol",
                format!(
                    "{}.{}",
                    info.version.version as u32 >> 16,
                    info.version.version as u32 & 0xffff
                ),
            );

            resp.binary("feature_flags", u32::from(info.version.feature_flags));
            resp.field("interrupt_page", info.interrupt_page);
            resp.field("modifying", info.modifying);
            resp.field("client_id", info.client_id);
            trusted = info.trusted;
        }

        let next_action = next_action.map(|a| match a {
            ConnectionAction::None => "disconnect",
            ConnectionAction::Reset => "reset",
            ConnectionAction::SendUnloadComplete => "unload",
            ConnectionAction::Reconnect { .. } => "reconnect",
            ConnectionAction::SendFailedVersionResponse => "send_version_response",
        });
        resp.field("state", state)
            .field("trusted", trusted)
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

#[derive(Debug, Copy, Clone)]
struct ConnectionInfo {
    version: VersionInfo,
    // Indicates if the connection is trusted for the paravisor of a hardware-isolated VM. In other
    // cases, this value is always false.
    trusted: bool,
    offers_sent: bool,
    interrupt_page: Option<u64>,
    monitor_page: Option<MonitorPageGpas>,
    target_message_vp: u32,
    modifying: bool,
    client_id: Guid,
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

    fn is_trusted(&self) -> bool {
        match self {
            ConnectionState::Connected(info) => info.trusted,
            ConnectionState::Connecting { info, .. } => info.trusted,
            _ => false,
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
    pub flags: u16,
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
    pub version: Option<u32>,
    pub monitor_page: Update<MonitorPageGpas>,
    pub interrupt_page: Update<u64>,
    pub target_message_vp: Option<u32>,
    pub force: bool,
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
            force: false,
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
    /// No version change was was requested, or the requested version is supported. Includes all the
    /// feature flags supported by the relay host, so that supported flags reported to the guest can
    /// be limited to that. The FeatureFlags field is not relevant if no version change was
    /// requested.
    Supported(protocol::ConnectionState, FeatureFlags),
    /// A version change was requested but the relay host doesn't support that version. This
    /// response cannot be returned for a request with no version change set.
    Unsupported,
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
    /// but restore_channel has not yet been called on it, and post_restore has
    /// not yet been called.
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

#[derive(Debug, Clone, Default, mesh::MeshPayload)]
pub struct OfferParamsInternal {
    /// An informational string describing the channel type.
    pub interface_name: String,
    pub instance_id: Guid,
    pub interface_id: Guid,
    pub mmio_megabytes: u16,
    pub mmio_megabytes_optional: u16,
    pub subchannel_index: u16,
    pub use_mnf: bool,
    pub offer_order: Option<u32>,
    pub flags: OfferFlags,
    pub user_defined: UserDefinedData,
    pub monitor_id: Option<u8>,
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
            use_mnf: value.use_mnf,
            offer_order: value.offer_order,
            user_defined,
            flags,
            monitor_id: None,
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
    ReservedChannel(OfferId),
    Custom(ConnectionTarget),
}

impl MessageTarget {
    pub fn for_offer(offer_id: OfferId, reserved: bool) -> Self {
        if reserved {
            Self::ReservedChannel(offer_id)
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

    /// Returns the monitor ID only if it's being handled by this server.
    ///
    /// The monitor ID can be set while use_mnf is false, which is the case if
    /// the relay host is handling MNF.
    ///
    /// Also returns `None` for reserved channels, since monitored notifications
    /// are only usable for standard channels. Otherwise, we fail later when we
    /// try to change the MNF page as part of vmbus protocol renegotiation,
    /// since the page still appears to be in use by a device.
    fn handled_monitor_id(&self) -> Option<MonitorId> {
        if self.offer.use_mnf && !self.state.is_reserved() {
            self.info.and_then(|info| info.monitor_id)
        } else {
            None
        }
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
        let connection_id = ConnectionId::new(channel_id.0, assigned_channels.vtl, SINT);

        // Allocate a monitor ID if the channel uses MNF.
        // N.B. If the synic doesn't support MNF or MNF is disabled by the server, use_mnf should
        //      always be set to false. For the relay, that means the host is handling MNF so we
        //      should use the monitor ID it provided if there is one.
        let monitor_id = if self.offer.use_mnf {
            let monitor_id = assigned_monitors.assign_monitor();
            if monitor_id.is_none() {
                tracelimit::warn_ratelimited!("Out of monitor IDs.");
            }

            monitor_id
        } else {
            self.offer.monitor_id.map(MonitorId)
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

            // Only unassign the monitor ID if it was not explicitly provided by the offer.
            if let Some(monitor_id) = info.monitor_id {
                if self.offer.use_mnf {
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
        assert!(self.list.assignments[self.index]
            .replace(offer_id)
            .is_none());

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
                    if !matches!(channel.state, ChannelState::Revoked) {
                        notifier.inspect(version, offer_id, resp.request());
                    }
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
                gparange::MultiPagedRangeBuf::<Vec<u64>>::validate(self.count as usize, buf)
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
    pub monitor_id: Option<MonitorId>,
    pub flags: protocol::OpenChannelFlags,
    pub reserved_target: Option<ConnectionTarget>,
}

impl OpenParams {
    fn from_request(
        info: &OfferedInfo,
        request: &OpenRequest,
        monitor_id: Option<MonitorId>,
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
            monitor_id,
            flags: protocol::OpenChannelFlags::from(request.flags).with_unused(0),
            reserved_target,
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

/// An error that occurred while mapping the interrupt page.
#[derive(Error, Debug)]
pub enum InterruptPageError {
    #[error("memory")]
    MemoryError(#[from] GuestMemoryError),
    #[error("synic")]
    SynicError(#[from] vmcore::synic::Error),
    #[error("gpa {0:#x} is not page aligned")]
    NotPageAligned(u64),
}

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
    fn send_message(&mut self, message: OutgoingMessage, target: MessageTarget);

    /// Used to signal the hvsocket handler that there is a new connection request.
    fn notify_hvsock(&mut self, request: &HvsockConnectRequest);

    /// Notifies that a requested reset is complete.
    fn reset_complete(&mut self);

    /// Updates the message port for a reserved channel.
    fn update_reserved_channel(
        &mut self,
        offer_id: OfferId,
        target: ConnectionTarget,
    ) -> Result<(), ChannelError>;
}

impl Server {
    /// Creates a new VMBus server.
    pub fn new(vtl: Vtl, child_connection_id: u32, channel_id_offset: u16) -> Self {
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
                return Err(RestoreError::AlreadyRestored(channel.offer.key()))
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
            channel.handled_monitor_id(),
            reserved_state.map(|state| state.target),
        ))
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
                return Err(RestoreError::AlreadyRestored(channel.offer.key()))
            }
        }

        let info = channel
            .info
            .ok_or_else(|| RestoreError::MissingChannel(channel.offer.key()))?;

        if let Some(monitor_id) = channel.handled_monitor_id() {
            if !self.inner.assigned_monitors.claim_monitor(monitor_id) {
                return Err(RestoreError::DuplicateMonitorId(monitor_id.0));
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
                    send_open_result(
                        self.notifier,
                        info.channel_id,
                        &request,
                        protocol::STATUS_SUCCESS,
                        MessageTarget::for_offer(offer_id, reserved_state.is_some()),
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
                                channel.handled_monitor_id(),
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
                                channel.handled_monitor_id(),
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

    pub fn post_restore(&mut self) -> Result<(), RestoreError> {
        for (offer_id, channel) in self.inner.channels.iter_mut() {
            match channel.restore_state {
                RestoreState::Restored => {
                    // The channel is fully restored. Nothing more to do.
                }
                RestoreState::New => {
                    // This is a fresh channel offer, not in the saved state.
                    // Send the offer to the guest if it has not already been
                    // sent (which could have happened if the channel was
                    // offered after restore() but before post_restore()).
                    if let ConnectionState::Connected(info) = &self.inner.state {
                        if matches!(channel.state, ChannelState::ClientReleased) {
                            channel.prepare_channel(
                                offer_id,
                                &mut self.inner.assigned_channels,
                                &mut self.inner.assigned_monitors,
                            );
                            channel.state = ChannelState::Closed;
                            send_offer(self.notifier, channel, info.version);
                        }
                    }
                }
                RestoreState::Restoring => {
                    // restore_channel was never called for this, but it was in
                    // the saved state. This indicates the offer is meant to be
                    // fresh, so revoke and reoffer it.
                    let retain = revoke(offer_id, channel, &mut self.inner.gpadls, self.notifier);
                    assert!(retain, "channel has not been released");
                    channel.state = ChannelState::Reoffered;
                }
                RestoreState::Unmatched => {
                    // offer_channel was never called for this, but it was in
                    // the saved state. Revoke it.
                    let retain = revoke(offer_id, channel, &mut self.inner.gpadls, self.notifier);
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

        // Restore server state, and resend server notifications if needed. If these notifications
        // were processed before the save, it's harmless as the values will be the same.
        let request = match self.inner.state {
            ConnectionState::Connecting {
                info,
                next_action: _,
            } => Some(ModifyConnectionRequest {
                version: Some(info.version.version as u32),
                interrupt_page: info.interrupt_page.into(),
                monitor_page: info.monitor_page.into(),
                target_message_vp: Some(info.target_message_vp),
                force: true,
                notify_relay: true,
            }),
            ConnectionState::Connected(info) => Some(ModifyConnectionRequest {
                version: None,
                monitor_page: info.monitor_page.into(),
                interrupt_page: info.interrupt_page.into(),
                target_message_vp: Some(info.target_message_vp),
                force: true,
                // If the save didn't happen while modifying, the relay doesn't need to be notified
                // of this info as it doesn't constitute a change, we're just restoring existing
                // connection state.
                notify_relay: info.modifying,
            }),
            // No action needed for these states; if disconnecting, check_disconnected will resend
            // the reset request if needed.
            ConnectionState::Disconnected | ConnectionState::Disconnecting { .. } => None,
        };

        if let Some(request) = request {
            self.notifier.modify_connection(request)?;
        }

        self.check_disconnected();
        Ok(())
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
        self.notifier.reset_complete();
    }

    /// Creates a new channel, returning its channel ID.
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
                // restore_channel and post_restore.
                channel.restore_state = RestoreState::Restoring;

                // The relay can specify a host-determined monitor ID, which needs to match what's
                // in the saved state.
                if let Some(monitor_id) = offer.monitor_id {
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

        let mut connected_version = None;
        let state = match self.inner.state {
            ConnectionState::Connected(ConnectionInfo {
                offers_sent: true,
                version,
                ..
            }) => {
                connected_version = Some(version);
                ChannelState::Closed
            }
            ConnectionState::Connected(ConnectionInfo {
                offers_sent: false, ..
            })
            | ConnectionState::Connecting { .. }
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
        if let Some(version) = connected_version {
            let channel = &mut self.inner.channels[offer_id];
            channel.prepare_channel(
                offer_id,
                &mut self.inner.assigned_channels,
                &mut self.inner.assigned_monitors,
            );

            send_offer(self.notifier, channel, version);
        }

        tracing::info!(?offer_id, %key, confidential_ring_buffer, confidential_external_memory, "new channel");
        Ok(offer_id)
    }

    /// Revokes a channel by ID.
    pub fn revoke_channel(&mut self, offer_id: OfferId) {
        let channel = &mut self.inner.channels[offer_id];
        let retain = revoke(
            offer_id,
            channel,
            &mut self.inner.gpadls,
            &mut *self.notifier,
        );
        if !retain {
            self.inner.channels.remove(offer_id);
        }

        self.check_disconnected();
    }

    /// Completes an open operation with `result`.
    pub fn open_complete(&mut self, offer_id: OfferId, result: i32) {
        tracing::debug!(offer_id = offer_id.0, result, "open complete");

        let channel = &mut self.inner.channels[offer_id];
        match channel.state {
            ChannelState::Opening {
                request,
                reserved_state,
            } => {
                let channel_id = channel.info.expect("assigned").channel_id;
                tracing::info!(
                    offer_id = offer_id.0,
                    channel_id = channel_id.0,
                    result,
                    "opened channel"
                );

                send_open_result(
                    self.notifier,
                    channel_id,
                    &request,
                    result,
                    MessageTarget::for_offer(offer_id, reserved_state.is_some()),
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
                tracing::error!(?offer_id, state = ?channel.state, "invalid open complete")
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
            }
            ConnectionState::Disconnecting {
                modify_sent: true, ..
            }
            | ConnectionState::Disconnected
            | ConnectionState::Connected { .. }
            | ConnectionState::Connecting { .. } => (),
        }
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
        tracing::info!(offer_id = offer_id.0, "closed channel");
        match channel.state {
            ChannelState::Closing {
                reserved_state: Some(ReservedState { .. }),
                ..
            } => {
                channel.state = ChannelState::Closed;
                if matches!(self.inner.state, ConnectionState::Connected { .. }) {
                    let channel_id = channel.info.expect("assigned").channel_id;
                    self.send_close_reserved_channel_response(channel_id, offer_id);
                } else {
                    // Handle closing reserved channels while disconnected/ing. Since we weren't waiting
                    // on the channel, no need to call check_disconnected, but we do need to release it.
                    if Self::client_release_channel(
                        self.notifier,
                        offer_id,
                        channel,
                        &mut self.inner.gpadls,
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
                tracing::error!(?offer_id, state = ?channel.state, "invalid close complete")
            }
        }
    }

    fn send_close_reserved_channel_response(&mut self, channel_id: ChannelId, offer_id: OfferId) {
        send_message_with_target(
            self.notifier,
            &protocol::CloseReservedChannelResponse { channel_id },
            MessageTarget::ReservedChannel(offer_id),
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
            SINT
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

        if request.target_sint != SINT {
            tracelimit::warn_ratelimited!(
                "unsupported multiclient request for VTL {} SINT {}, version {:#x}",
                request.target_vtl,
                request.target_sint,
                request.version_requested,
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
            MonitorPageRequest::Some(mp) => Some(mp),
            MonitorPageRequest::None => None,
            MonitorPageRequest::Invalid => {
                // Do not notify the relay in this case.
                self.send_version_response(Some((
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
                monitor_page,
                target_message_vp: request.target_message_vp,
                modifying: false,
                offers_sent: false,
                client_id: request.client_id,
            },
            next_action: ConnectionAction::None,
        };

        // Update server state and notify the relay, if any. When complete,
        // complete_initiate_contact will be invoked.
        if let Err(err) = self.notifier.modify_connection(ModifyConnectionRequest {
            version: Some(request.version_requested),
            monitor_page: monitor_page.into(),
            interrupt_page: request.interrupt_page.into(),
            target_message_vp: Some(request.target_message_vp),
            force: false,
            notify_relay: true,
        }) {
            tracelimit::error_ratelimited!(?err, "server failed to change state");
            self.inner.state = ConnectionState::Disconnected;
            self.send_version_response(Some((
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
        const LOCAL_FEATURE_FLAGS: FeatureFlags = FeatureFlags::new()
            .with_client_id(true)
            .with_confidential_channels(true);

        let relay_feature_flags = match response {
            // There is no relay, or it successfully processed our request.
            ModifyConnectionResponse::Supported(
                protocol::ConnectionState::SUCCESSFUL,
                feature_flags,
            ) => feature_flags,
            // The relay supports the requested version, but encountered an error, so pass it
            // along to the guest.
            ModifyConnectionResponse::Supported(connection_state, feature_flags) => {
                tracelimit::error_ratelimited!(
                    ?connection_state,
                    "initiate contact failed because relay request failed"
                );

                // We still report the supported feature flags with an error, so make sure those
                // are correct.
                info.version.feature_flags &= feature_flags | LOCAL_FEATURE_FLAGS;

                self.send_version_response(Some((info.version, connection_state)));
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
        };

        // The relay responds with all the feature flags it supports, so limit the flags reported to
        // the guest to include only those handled by the relay or locally.
        info.version.feature_flags &= relay_feature_flags | LOCAL_FEATURE_FLAGS;
        self.inner.state = ConnectionState::Connected(info);

        self.send_version_response(Some((info.version, protocol::ConnectionState::SUCCESSFUL)));
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

        let supported_flags = if version >= Version::Copper {
            // The max version and features may be limited in order to test older protocol versions.
            //
            // N.B. Confidential channels should only be enabled if the connection is trusted.
            if let Some(max_version) = self.inner.max_version {
                if version as u32 > max_version.version {
                    return None;
                }

                max_version.feature_flags.with_confidential_channels(
                    max_version.feature_flags.confidential_channels() && request.trusted,
                )
            } else {
                FeatureFlags::all().with_confidential_channels(request.trusted)
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

    fn send_version_response(&mut self, data: Option<(VersionInfo, protocol::ConnectionState)>) {
        self.send_version_response_with_target(data, MessageTarget::Default);
    }

    fn send_version_response_with_target(
        &mut self,
        data: Option<(VersionInfo, protocol::ConnectionState)>,
        target: MessageTarget,
    ) {
        let mut response2 = protocol::VersionResponse2::new_zeroed();
        let response = &mut response2.version_response;
        let mut send_response2 = false;
        if let Some((version, state)) = data {
            // Pre-Win8, there is no way to report failures to the guest, so those should be treated
            // as unsupported.
            if state == protocol::ConnectionState::SUCCESSFUL || version.version >= Version::Win8 {
                response.version_supported = 1;
                response.connection_state = state;
                response.selected_version_or_connection_id =
                    if version.version >= Version::Win10Rs3_1 {
                        self.inner.child_connection_id
                    } else {
                        version.version as u32
                    };

                if version.version >= Version::Copper {
                    response2.supported_features = version.feature_flags.into();
                    send_response2 = true;
                }
            }
        }

        if send_response2 {
            send_message_with_target(self.notifier, &response2, target);
        } else {
            send_message_with_target(self.notifier, response, target);
        }
    }

    /// Disconnects the guest, putting the server into `new_state` and returning
    /// false if there are channels that are not yet fully reset.
    fn request_disconnect(&mut self, new_action: ConnectionAction) -> bool {
        assert!(!self.is_resetting());

        // Release all channels.
        let gpadls = &mut self.inner.gpadls;
        let notifier = &mut self.notifier;
        let vm_reset = matches!(new_action, ConnectionAction::Reset);
        self.inner.channels.retain(|offer_id, channel| {
            // Release reserved channels only if the VM is resetting
            (!vm_reset && channel.state.is_reserved())
                || !Self::client_release_channel(
                    notifier,
                    offer_id,
                    channel,
                    gpadls,
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
                    self.inner.state = ConnectionState::Disconnected;
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
        if let Some(version) = self.inner.delayed_max_version.take() {
            self.inner.set_compatibility_version(version, false);
        }

        send_message(self.notifier, &protocol::UnloadComplete {});
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

        // The guest expects channel IDs to stay consistent across hibernation and
        // resume, so sort the current offers before assigning channel IDs.
        let mut sorted_channels: Vec<_> = self
            .inner
            .channels
            .iter_mut()
            .filter(|(_, channel)| !channel.state.is_reserved())
            .collect();

        sorted_channels.sort_unstable_by_key(|(_, channel)| {
            (
                channel.offer.interface_id,
                channel.offer.offer_order.unwrap_or(u32::MAX),
                channel.offer.instance_id,
            )
        });

        for (offer_id, channel) in sorted_channels {
            assert!(matches!(channel.state, ChannelState::ClientReleased));
            assert!(channel.info.is_none());

            channel.prepare_channel(
                offer_id,
                &mut self.inner.assigned_channels,
                &mut self.inner.assigned_monitors,
            );

            channel.state = ChannelState::Closed;
            send_offer(self.notifier, channel, info.version);
        }
        send_message(self.notifier, &protocol::AllOffersDelivered {});

        Ok(())
    }

    /// Sends a GPADL to the device when `ranges` is Some. Returns false if the
    /// GPADL should be removed because the channel is already revoked.
    #[must_use]
    fn gpadl_updated(
        notifier: &mut N,
        offer_id: OfferId,
        channel: &Channel,
        gpadl_id: GpadlId,
        gpadl: &Gpadl,
    ) -> bool {
        if channel.state.is_revoked() {
            let channel_id = channel.info.as_ref().expect("assigned").channel_id;
            send_gpadl_created(
                notifier,
                channel_id,
                gpadl_id,
                protocol::STATUS_UNSUCCESSFUL,
            );
            false
        } else {
            // Notify the channel if the GPADL is done.
            notifier.notify(
                offer_id,
                Action::Gpadl(gpadl_id, gpadl.count, gpadl.buf.clone()),
            );
            true
        }
    }

    /// Handles MessageType::GPADL_HEADER, which creates a new GPADL.
    fn handle_gpadl_header(
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
        if !done
            && self
                .inner
                .incomplete_gpadls
                .insert(input.gpadl_id, offer_id)
                .is_some()
        {
            unreachable!("gpadl ID validated above");
        }

        if done && !Self::gpadl_updated(self.notifier, offer_id, channel, input.gpadl_id, gpadl) {
            self.inner.gpadls.remove(&(input.gpadl_id, offer_id));
        }
        Ok(())
    }

    /// Handles MessageType::GPADL_BODY, which adds more to an in-progress
    /// GPADL.
    fn handle_gpadl_body(
        &mut self,
        input: &protocol::GpadlBody,
        range: &[u8],
    ) -> Result<(), ChannelError> {
        // Find and update the GPADL.
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

        if gpadl.append(range)? {
            self.inner.incomplete_gpadls.remove(&input.gpadl_id);
            if !Self::gpadl_updated(self.notifier, offer_id, channel, input.gpadl_id, gpadl) {
                self.inner.gpadls.remove(&(input.gpadl_id, offer_id));
            }
        }

        Ok(())
    }

    /// Handles MessageType::GPADL_TEARDOWN, which tears down a GPADL.
    fn handle_gpadl_teardown(
        &mut self,
        input: &protocol::GpadlTeardown,
    ) -> Result<(), ChannelError> {
        tracing::debug!(
            channel_id = input.channel_id.0,
            gpadl_id = input.gpadl_id.0,
            "Received GPADL teardown request"
        );

        let (offer_id, channel) = self
            .inner
            .channels
            .get_by_channel_id_mut(&self.inner.assigned_channels, input.channel_id)?;

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
                        gpadl_id = input.gpadl_id.0,
                        "Gpadl teardown for revoked channel"
                    );

                    self.inner.gpadls.remove(&(input.gpadl_id, offer_id));
                    send_gpadl_torndown(self.notifier, input.gpadl_id);
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
                    channel.handled_monitor_id(),
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
            0
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
            flags: 0,
        };

        match channel.state {
            ChannelState::Closed => self.open_channel(offer_id, &request, reserved_state),
            ChannelState::Revoked | ChannelState::Reoffered => {}

            ChannelState::Open { .. } | ChannelState::Opening { .. } => {
                return Err(ChannelError::ChannelAlreadyOpen)
            }

            ChannelState::Closing { .. } | ChannelState::ClosingReopen { .. } => {
                return Err(ChannelError::InvalidChannelState)
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
                if resvd.target.vp != input.target_vp
                    || resvd.target.sint != input.target_sint as u8
                {
                    resvd.target.vp = input.target_vp;
                    resvd.target.sint = input.target_sint as u8;
                    self.notifier
                        .update_reserved_channel(offer_id, resvd.target)?;
                }

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
        notifier: &mut N,
        offer_id: OfferId,
        channel: &mut Channel,
        gpadls: &mut GpadlMap,
        assigned_channels: &mut AssignedChannels,
        assigned_monitors: &mut AssignedMonitors,
        version: Option<VersionInfo>,
    ) -> bool {
        // Release any GPADLs that remain for this channel.
        gpadls.retain(|&(gpadl_id, gpadl_offer_id), gpadl| {
            if gpadl_offer_id != offer_id {
                return true;
            }
            match gpadl.state {
                GpadlState::InProgress => false,
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
                        notifier.notify(
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
                if let Some(version) = version {
                    channel.state = ChannelState::Closed;
                    channel.restore_state = RestoreState::New;
                    send_offer(notifier, channel, version);
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
                notifier.notify(offer_id, Action::Close);
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
                    self.notifier,
                    offer_id,
                    channel,
                    &mut self.inner.gpadls,
                    &mut self.inner.assigned_channels,
                    &mut self.inner.assigned_monitors,
                    self.inner.state.get_version(),
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
        self.notifier.notify_hvsock(&request.into());
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
            send_message(
                self.notifier,
                &protocol::TlConnectResult {
                    service_id: result.service_id,
                    endpoint_id: result.endpoint_id,
                    status: protocol::STATUS_CONNECTION_REFUSED,
                },
            )
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
            self.send_modify_channel_response(channel_id, status);

            // Handle a pending ModifyChannel request if there is one.
            if let Some(target_vp) = pending_target_vp {
                let request = protocol::ModifyChannel {
                    channel_id,
                    target_vp,
                };

                if let Err(error) = self.handle_modify_channel(&request) {
                    tracelimit::warn_ratelimited!(?error, "Pending ModifyChannel request failed.")
                }
            }
        }
    }

    fn send_modify_channel_response(&mut self, channel_id: ChannelId, status: i32) {
        if self.inner.state.check_version(Version::Iron) {
            send_message(
                self.notifier,
                &protocol::ModifyChannelResponse { channel_id, status },
            );
        }
    }

    fn handle_modify_connection(&mut self, request: protocol::ModifyConnection) {
        if let Err(err) = self.modify_connection(request) {
            tracelimit::error_ratelimited!(?err, "modifying connection failed");
            self.complete_modify_connection(ModifyConnectionResponse::Supported(
                protocol::ConnectionState::FAILED_UNKNOWN_FAILURE,
                FeatureFlags::new(),
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

        if (request.child_to_parent_monitor_page_gpa == 0)
            != (request.parent_to_child_monitor_page_gpa == 0)
        {
            anyhow::bail!("Guest must specify either both or no monitor pages, {request:?}");
        }

        let monitor_page =
            (request.child_to_parent_monitor_page_gpa != 0).then_some(MonitorPageGpas {
                child_to_parent: request.child_to_parent_monitor_page_gpa,
                parent_to_child: request.parent_to_child_monitor_page_gpa,
            });

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
                let ModifyConnectionResponse::Supported(connection_state, ..) = response else {
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
                send_message(
                    self.notifier,
                    &protocol::ModifyConnectionResponse { connection_state },
                );
            }
            _ => panic!(
                "Invalid state for ModifyConnection response: {:?}",
                self.inner.state
            ),
        }
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
            tracing::warn!(?msg, "Received untrusted message");
            return Err(ChannelError::UntrustedMessage);
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
            Message::GpadlHeader(input, range) => self.handle_gpadl_header(&input, range)?,
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
            // Messages that should only be received by a vmbus client.
            Message::OfferChannel(..)
            | Message::RescindChannelOffer(..)
            | Message::AllOffersDelivered(..)
            | Message::OpenResult(..)
            | Message::GpadlCreated(..)
            | Message::GpadlTorndown(..)
            | Message::VersionResponse(..)
            | Message::VersionResponse2(..)
            | Message::UnloadComplete(..)
            | Message::CloseReservedChannelResponse(..)
            | Message::TlConnectResult(..)
            | Message::ModifyChannelResponse(..)
            | Message::ModifyConnectionResponse(..) => {
                unreachable!("Server received client message {:?}", msg);
            }
        }
        Ok(())
    }

    fn get_gpadl(
        gpadls: &mut GpadlMap,
        offer_id: OfferId,
        gpadl_id: GpadlId,
    ) -> Option<&mut Gpadl> {
        let gpadl = gpadls.get_mut(&(gpadl_id, offer_id));
        if gpadl.is_none() {
            tracelimit::error_ratelimited!(?offer_id, ?gpadl_id, "invalid gpadl ID for channel");
        }
        gpadl
    }

    /// Completes a GPADL creation, accepting it if `status >= 0`, rejecting it otherwise.
    pub fn gpadl_create_complete(&mut self, offer_id: OfferId, gpadl_id: GpadlId, status: i32) {
        let gpadl = if let Some(gpadl) = Self::get_gpadl(&mut self.inner.gpadls, offer_id, gpadl_id)
        {
            gpadl
        } else {
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
                send_gpadl_created(self.notifier, channel_id, gpadl_id, status);
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
        tracing::debug!(
            offer_id = offer_id.0,
            gpadl_id = gpadl_id.0,
            "Gpadl teardown complete"
        );

        let gpadl = if let Some(gpadl) = Self::get_gpadl(&mut self.inner.gpadls, offer_id, gpadl_id)
        {
            gpadl
        } else {
            return;
        };
        let channel = &mut self.inner.channels[offer_id];
        match gpadl.state {
            GpadlState::InProgress
            | GpadlState::Offered
            | GpadlState::OfferedTearingDown
            | GpadlState::Accepted => {
                tracelimit::error_ratelimited!(?offer_id, ?gpadl_id, ?gpadl, "invalid gpadl state");
            }
            GpadlState::TearingDown => {
                if !channel.state.is_released() {
                    send_gpadl_torndown(self.notifier, gpadl_id);
                }
                self.inner
                    .gpadls
                    .remove(&(gpadl_id, offer_id))
                    .expect("gpadl validated above");

                self.check_disconnected();
            }
        }
    }
}

fn revoke<N: Notifier>(
    offer_id: OfferId,
    channel: &mut Channel,
    gpadls: &mut GpadlMap,
    notifier: &mut N,
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
                    send_gpadl_created(
                        notifier,
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
                    send_gpadl_torndown(notifier, gpadl_id);
                }
                false
            }
        }
    });
    if let Some(info) = info {
        send_rescind(notifier, info);
    }
    // Revoking a channel effectively completes the restore operation for it.
    if channel.restore_state != RestoreState::New {
        channel.restore_state = RestoreState::Restored;
    }
    retain
}

/// Sends a VMBus channel message to the guest.
fn send_message<
    N: Notifier,
    T: IntoBytes + protocol::VmbusMessage + std::fmt::Debug + Immutable + KnownLayout,
>(
    notifier: &mut N,
    msg: &T,
) {
    send_message_with_target(notifier, msg, MessageTarget::Default);
}

/// Sends a VMBus channel message to the guest via an alternate port.
fn send_message_with_target<
    N: Notifier,
    T: IntoBytes + protocol::VmbusMessage + std::fmt::Debug + Immutable + KnownLayout,
>(
    notifier: &mut N,
    msg: &T,
    target: MessageTarget,
) {
    tracing::trace!(typ = ?T::MESSAGE_TYPE, ?msg, "sending message");
    notifier.send_message(OutgoingMessage::new(msg), target);
}

/// Sends a channel offer message to the guest.
fn send_offer<N: Notifier>(notifier: &mut N, channel: &mut Channel, version: VersionInfo) {
    let info = channel.info.as_ref().expect("assigned");
    let mut flags = channel.offer.flags;
    if !version.feature_flags.confidential_channels() {
        flags.set_confidential_ring_buffer(false);
        flags.set_confidential_external_memory(false);
    }

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
        monitor_id: info.monitor_id.unwrap_or(MonitorId::INVALID).0,
        monitor_allocated: info.monitor_id.is_some() as u8,
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

    send_message(notifier, &msg);
}

fn send_open_result<N: Notifier>(
    notifier: &mut N,
    channel_id: ChannelId,
    open_request: &OpenRequest,
    result: i32,
    target: MessageTarget,
) {
    send_message_with_target(
        notifier,
        &protocol::OpenResult {
            channel_id,
            open_id: open_request.open_id,
            status: result as u32,
        },
        target,
    );
}

fn send_gpadl_created<N: Notifier>(
    notifier: &mut N,
    channel_id: ChannelId,
    gpadl_id: GpadlId,
    status: i32,
) {
    send_message(
        notifier,
        &protocol::GpadlCreated {
            channel_id,
            gpadl_id,
            status,
        },
    );
}

fn send_gpadl_torndown<N: Notifier>(notifier: &mut N, gpadl_id: GpadlId) {
    send_message(notifier, &protocol::GpadlTorndown { gpadl_id });
}

fn send_rescind<N: Notifier>(notifier: &mut N, info: &OfferedInfo) {
    tracing::info!(
        channel_id = info.channel_id.0,
        "rescinding channel from guest"
    );

    send_message(
        notifier,
        &protocol::RescindChannelOffer {
            channel_id: info.channel_id,
        },
    );
}

#[cfg(test)]
mod tests {
    use crate::MESSAGE_CONNECTION_ID;

    use super::*;
    use guid::Guid;
    use protocol::VmbusMessage;
    use std::collections::VecDeque;
    use std::sync::mpsc;
    use test_with_tracing::test;
    use vmbus_core::protocol::TargetInfo;
    use zerocopy::FromBytes;

    fn in_msg<T: IntoBytes + Immutable + KnownLayout>(
        message_type: protocol::MessageType,
        t: T,
    ) -> SynicMessage {
        in_msg_ex(message_type, t, false, false)
    }

    fn in_msg_ex<T: IntoBytes + Immutable + KnownLayout>(
        message_type: protocol::MessageType,
        t: T,
        multiclient: bool,
        trusted: bool,
    ) -> SynicMessage {
        let mut data = Vec::new();
        data.extend_from_slice(&message_type.0.to_ne_bytes());
        data.extend_from_slice(&0u32.to_ne_bytes());
        data.extend_from_slice(t.as_bytes());
        SynicMessage {
            data,
            multiclient,
            trusted,
        }
    }

    #[test]
    fn test_version_negotiation_not_supported() {
        let (mut notifier, _recv) = TestNotifier::new();
        let mut server = Server::new(Vtl::Vtl0, MESSAGE_CONNECTION_ID, 0);

        test_initiate_contact(&mut server, &mut notifier, 0xffffffff, 0, false, 0);
    }

    #[test]
    fn test_version_negotiation_success() {
        let (mut notifier, _recv) = TestNotifier::new();
        let mut server = Server::new(Vtl::Vtl0, MESSAGE_CONNECTION_ID, 0);

        test_initiate_contact(
            &mut server,
            &mut notifier,
            Version::Win10 as u32,
            0,
            true,
            0,
        );
    }

    #[test]
    fn test_version_negotiation_multiclient_sint() {
        let (mut notifier, _recv) = TestNotifier::new();
        let mut server = Server::new(Vtl::Vtl0, MESSAGE_CONNECTION_ID, 0);

        let target_info = TargetInfo::new()
            .with_sint(3)
            .with_vtl(0)
            .with_feature_flags(FeatureFlags::new().into());

        server
            .with_notifier(&mut notifier)
            .handle_synic_message(in_msg_ex(
                protocol::MessageType::INITIATE_CONTACT,
                protocol::InitiateContact {
                    version_requested: Version::Win10Rs3_1 as u32,
                    target_message_vp: 0,
                    interrupt_page_or_target_info: target_info.into(),
                    parent_to_child_monitor_page_gpa: 0,
                    child_to_parent_monitor_page_gpa: 0,
                },
                true,
                false,
            ))
            .unwrap();

        // No action is taken when a different SINT is requested, since it's not supported. An
        // unsupported message is sent to the requested SINT.
        assert!(notifier.modify_requests.is_empty());
        assert!(matches!(server.state, ConnectionState::Disconnected));
        notifier.check_message_with_target(
            OutgoingMessage::new(&protocol::VersionResponse {
                version_supported: 0,
                connection_state: protocol::ConnectionState::SUCCESSFUL,
                padding: 0,
                selected_version_or_connection_id: 0,
            }),
            MessageTarget::Custom(ConnectionTarget { vp: 0, sint: 3 }),
        );

        // SINT is ignored if the multiclient port is not used.
        test_initiate_contact(
            &mut server,
            &mut notifier,
            Version::Win10Rs3_1 as u32,
            target_info.into(),
            true,
            0,
        );
    }

    #[test]
    fn test_version_negotiation_multiclient_vtl() {
        let (mut notifier, _recv) = TestNotifier::new();
        let mut server = Server::new(Vtl::Vtl0, MESSAGE_CONNECTION_ID, 0);

        let target_info = TargetInfo::new()
            .with_sint(SINT)
            .with_vtl(2)
            .with_feature_flags(FeatureFlags::new().into());

        server
            .with_notifier(&mut notifier)
            .handle_synic_message(in_msg_ex(
                protocol::MessageType::INITIATE_CONTACT,
                protocol::InitiateContact {
                    version_requested: Version::Win10Rs4 as u32,
                    target_message_vp: 0,
                    interrupt_page_or_target_info: target_info.into(),
                    parent_to_child_monitor_page_gpa: 0,
                    child_to_parent_monitor_page_gpa: 0,
                },
                true,
                false,
            ))
            .unwrap();

        let action = notifier.forward_request.take().unwrap();
        assert!(matches!(action, InitiateContactRequest { .. }));

        // The VTL contact message was forwarded but no action was taken by this server.
        assert!(notifier.messages.is_empty());
        assert!(matches!(server.state, ConnectionState::Disconnected));

        // VTL is ignored if the multiclient port is not used.
        test_initiate_contact(
            &mut server,
            &mut notifier,
            Version::Win10Rs4 as u32,
            target_info.into(),
            true,
            0,
        );

        assert!(notifier.forward_request.is_none());
    }

    #[test]
    fn test_version_negotiation_feature_flags() {
        let (mut notifier, _recv) = TestNotifier::new();
        let mut server = Server::new(Vtl::Vtl0, MESSAGE_CONNECTION_ID, 0);

        // Test with no feature flags.
        let mut target_info = TargetInfo::new()
            .with_sint(SINT)
            .with_vtl(0)
            .with_feature_flags(FeatureFlags::new().into());
        test_initiate_contact(
            &mut server,
            &mut notifier,
            Version::Copper as u32,
            target_info.into(),
            true,
            0,
        );

        // Request supported feature flags.
        target_info.set_feature_flags(
            FeatureFlags::new()
                .with_guest_specified_signal_parameters(true)
                .into(),
        );
        test_initiate_contact(
            &mut server,
            &mut notifier,
            Version::Copper as u32,
            target_info.into(),
            true,
            FeatureFlags::new()
                .with_guest_specified_signal_parameters(true)
                .into(),
        );

        // Request unsupported feature flags. This will succeed and report back the supported ones.
        target_info.set_feature_flags(
            u32::from(FeatureFlags::new().with_guest_specified_signal_parameters(true))
                | 0xf0000000,
        );
        test_initiate_contact(
            &mut server,
            &mut notifier,
            Version::Copper as u32,
            target_info.into(),
            true,
            FeatureFlags::new()
                .with_guest_specified_signal_parameters(true)
                .into(),
        );

        // Verify client ID feature flag.
        target_info.set_feature_flags(FeatureFlags::new().with_client_id(true).into());
        test_initiate_contact(
            &mut server,
            &mut notifier,
            Version::Copper as u32,
            target_info.into(),
            true,
            FeatureFlags::new().with_client_id(true).into(),
        );
    }

    #[test]
    fn test_version_negotiation_interrupt_page() {
        let (mut notifier, _recv) = TestNotifier::new();
        let mut server = Server::new(Vtl::Vtl0, MESSAGE_CONNECTION_ID, 0);
        test_initiate_contact(
            &mut server,
            &mut notifier,
            Version::V1 as u32,
            1234,
            true,
            0,
        );

        let (mut notifier, _recv) = TestNotifier::new();
        let mut server = Server::new(Vtl::Vtl0, MESSAGE_CONNECTION_ID, 0);
        test_initiate_contact(
            &mut server,
            &mut notifier,
            Version::Win7 as u32,
            1234,
            true,
            0,
        );

        let (mut notifier, _recv) = TestNotifier::new();
        let mut server = Server::new(Vtl::Vtl0, MESSAGE_CONNECTION_ID, 0);
        test_initiate_contact(
            &mut server,
            &mut notifier,
            Version::Win8 as u32,
            1234,
            true,
            0,
        );
    }

    fn test_initiate_contact(
        server: &mut Server,
        notifier: &mut TestNotifier,
        version: u32,
        target_info: u64,
        expect_supported: bool,
        expected_features: u32,
    ) {
        server
            .with_notifier(notifier)
            .handle_synic_message(in_msg(
                protocol::MessageType::INITIATE_CONTACT,
                protocol::InitiateContact2 {
                    initiate_contact: protocol::InitiateContact {
                        version_requested: version,
                        target_message_vp: 1,
                        interrupt_page_or_target_info: target_info,
                        parent_to_child_monitor_page_gpa: 0,
                        child_to_parent_monitor_page_gpa: 0,
                    },
                    client_id: Guid::from_static_str("e6e6e6e6-e6e6-e6e6-e6e6-e6e6e6e6e6e6"),
                },
            ))
            .unwrap();

        let selected_version_or_connection_id = if expect_supported {
            let request = notifier.next_action();
            let interrupt_page = if version < Version::Win8 as u32 {
                Update::Set(target_info)
            } else {
                Update::Reset
            };

            let target_message_vp = if version < Version::Win8_1 as u32 {
                Some(0)
            } else {
                Some(1)
            };

            assert_eq!(
                request,
                ModifyConnectionRequest {
                    version: Some(version),
                    monitor_page: Update::Reset,
                    interrupt_page,
                    target_message_vp,
                    ..Default::default()
                }
            );

            server.with_notifier(notifier).complete_initiate_contact(
                ModifyConnectionResponse::Supported(
                    protocol::ConnectionState::SUCCESSFUL,
                    FeatureFlags::all(),
                ),
            );

            if version >= Version::Win10Rs3_1 as u32 {
                1
            } else {
                version
            }
        } else {
            0
        };

        let version_response = protocol::VersionResponse {
            version_supported: if expect_supported { 1 } else { 0 },
            connection_state: protocol::ConnectionState::SUCCESSFUL,
            padding: 0,
            selected_version_or_connection_id,
        };

        if version >= Version::Copper as u32 && expect_supported {
            notifier.check_message(OutgoingMessage::new(&protocol::VersionResponse2 {
                version_response,
                supported_features: expected_features,
            }));
        } else {
            notifier.check_message(OutgoingMessage::new(&version_response));
            assert_eq!(expected_features, 0);
        }

        assert!(notifier.messages.is_empty());
        if expect_supported {
            assert!(matches!(server.state, ConnectionState::Connected { .. }));
            if version < Version::Win8_1 as u32 {
                assert_eq!(Some(0), notifier.target_message_vp);
            } else {
                assert_eq!(Some(1), notifier.target_message_vp);
            }
        } else {
            assert!(matches!(server.state, ConnectionState::Disconnected));
            assert!(notifier.target_message_vp.is_none());
        }

        if version < Version::Win8 as u32 {
            assert_eq!(notifier.interrupt_page, Some(target_info));
        } else {
            assert!(notifier.interrupt_page.is_none());
        }
    }

    struct TestNotifier {
        send: mpsc::Sender<(OfferId, Action)>,
        modify_requests: VecDeque<ModifyConnectionRequest>,
        messages: VecDeque<(OutgoingMessage, MessageTarget)>,
        hvsock_requests: Vec<HvsockConnectRequest>,
        forward_request: Option<InitiateContactRequest>,
        interrupt_page: Option<u64>,
        reset: bool,
        monitor_page: Option<MonitorPageGpas>,
        target_message_vp: Option<u32>,
        reserved_channel_update: Option<(OfferId, ConnectionTarget)>,
    }

    impl TestNotifier {
        fn new() -> (Self, mpsc::Receiver<(OfferId, Action)>) {
            let (send, recv) = mpsc::channel();
            (
                Self {
                    send,
                    modify_requests: VecDeque::new(),
                    messages: VecDeque::new(),
                    hvsock_requests: Vec::new(),
                    forward_request: None,
                    interrupt_page: None,
                    reset: false,
                    monitor_page: None,
                    target_message_vp: None,
                    reserved_channel_update: None,
                },
                recv,
            )
        }

        fn check_message(&mut self, message: OutgoingMessage) {
            self.check_message_with_target(message, MessageTarget::Default);
        }

        fn check_message_with_target(&mut self, message: OutgoingMessage, target: MessageTarget) {
            assert_eq!(self.messages.pop_front().unwrap(), (message, target));
            assert!(self.messages.is_empty());
        }

        fn get_message<T: VmbusMessage + FromBytes + Immutable + KnownLayout>(&mut self) -> T {
            let (message, _) = self.messages.pop_front().unwrap();
            let (header, data) = protocol::MessageHeader::read_from_prefix(message.data()).unwrap();

            assert_eq!(header.message_type(), T::MESSAGE_TYPE);
            T::read_from_prefix(data).unwrap().0 // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        }

        fn check_messages(&mut self, messages: &[OutgoingMessage]) {
            let messages: Vec<_> = messages
                .iter()
                .map(|m| (m.clone(), MessageTarget::Default))
                .collect();
            assert_eq!(self.messages, messages.as_slice());
            self.messages.clear();
        }

        fn is_reset(&mut self) -> bool {
            std::mem::replace(&mut self.reset, false)
        }

        fn check_reset(&mut self) {
            assert!(self.is_reset());
            assert!(self.monitor_page.is_none());
            assert!(self.target_message_vp.is_none());
        }

        fn next_action(&mut self) -> ModifyConnectionRequest {
            self.modify_requests.pop_front().unwrap()
        }
    }

    impl Notifier for TestNotifier {
        fn notify(&mut self, offer_id: OfferId, action: Action) {
            tracing::debug!(?offer_id, ?action, "notify");
            self.send.send((offer_id, action)).unwrap()
        }

        fn forward_unhandled(&mut self, request: InitiateContactRequest) {
            assert!(self.forward_request.is_none());
            self.forward_request = Some(request);
        }

        fn modify_connection(&mut self, request: ModifyConnectionRequest) -> anyhow::Result<()> {
            match request.monitor_page {
                Update::Unchanged => (),
                Update::Reset => self.monitor_page = None,
                Update::Set(value) => self.monitor_page = Some(value),
            }

            if let Some(vp) = request.target_message_vp {
                self.target_message_vp = Some(vp);
            }

            match request.interrupt_page {
                Update::Unchanged => (),
                Update::Reset => self.interrupt_page = None,
                Update::Set(value) => self.interrupt_page = Some(value),
            }

            self.modify_requests.push_back(request);
            Ok(())
        }

        fn send_message(&mut self, message: OutgoingMessage, target: MessageTarget) {
            self.messages.push_back((message, target));
        }

        fn notify_hvsock(&mut self, request: &HvsockConnectRequest) {
            tracing::debug!(?request, "notify_hvsock");
            // There is no hvsocket listener, so just drop everything.
            // N.B. No HvsockConnectResult will be sent to indicate failure.
            self.hvsock_requests.push(*request);
        }

        fn reset_complete(&mut self) {
            self.monitor_page = None;
            self.target_message_vp = None;
            self.reset = true;
        }

        fn update_reserved_channel(
            &mut self,
            offer_id: OfferId,
            target: ConnectionTarget,
        ) -> Result<(), ChannelError> {
            assert!(self.reserved_channel_update.is_none());
            self.reserved_channel_update = Some((offer_id, target));
            Ok(())
        }
    }

    impl Drop for TestNotifier {
        fn drop(&mut self) {
            assert!(self.reserved_channel_update.is_none());
        }
    }

    #[test]
    fn test_channel_lifetime() {
        test_channel_lifetime_helper(Version::Win10Rs5, FeatureFlags::new());
    }

    #[test]
    fn test_channel_lifetime_iron() {
        test_channel_lifetime_helper(Version::Iron, FeatureFlags::new());
    }

    #[test]
    fn test_channel_lifetime_copper() {
        test_channel_lifetime_helper(Version::Copper, FeatureFlags::new());
    }

    #[test]
    fn test_channel_lifetime_copper_guest_signal() {
        test_channel_lifetime_helper(
            Version::Copper,
            FeatureFlags::new().with_guest_specified_signal_parameters(true),
        );
    }

    #[test]
    fn test_channel_lifetime_copper_open_flags() {
        test_channel_lifetime_helper(
            Version::Copper,
            FeatureFlags::new().with_channel_interrupt_redirection(true),
        );
    }

    fn test_channel_lifetime_helper(version: Version, feature_flags: FeatureFlags) {
        let (mut notifier, recv) = TestNotifier::new();
        let mut server = Server::new(Vtl::Vtl0, MESSAGE_CONNECTION_ID, 0);
        let interface_id = Guid::new_random();
        let instance_id = Guid::new_random();
        let offer_id = server
            .with_notifier(&mut notifier)
            .offer_channel(OfferParamsInternal {
                interface_name: "test".to_owned(),
                instance_id,
                interface_id,
                ..Default::default()
            })
            .unwrap();

        let mut target_info = TargetInfo::new()
            .with_sint(SINT)
            .with_vtl(2)
            .with_feature_flags(FeatureFlags::new().into());
        if version >= Version::Copper {
            target_info.set_feature_flags(feature_flags.into());
        }

        server
            .with_notifier(&mut notifier)
            .handle_synic_message(in_msg(
                protocol::MessageType::INITIATE_CONTACT,
                protocol::InitiateContact {
                    version_requested: version as u32,
                    target_message_vp: 0,
                    interrupt_page_or_target_info: target_info.into(),
                    parent_to_child_monitor_page_gpa: 0,
                    child_to_parent_monitor_page_gpa: 0,
                },
            ))
            .unwrap();

        let request = notifier.next_action();
        assert_eq!(
            request,
            ModifyConnectionRequest {
                version: Some(version as u32),
                monitor_page: Update::Reset,
                interrupt_page: Update::Reset,
                target_message_vp: Some(0),
                ..Default::default()
            }
        );

        server
            .with_notifier(&mut notifier)
            .complete_initiate_contact(ModifyConnectionResponse::Supported(
                protocol::ConnectionState::SUCCESSFUL,
                FeatureFlags::all(),
            ));

        let version_response = protocol::VersionResponse {
            version_supported: 1,
            selected_version_or_connection_id: 1,
            ..FromZeros::new_zeroed()
        };

        if version >= Version::Copper {
            notifier.check_message(OutgoingMessage::new(&protocol::VersionResponse2 {
                version_response,
                supported_features: feature_flags.into(),
            }));
        } else {
            notifier.check_message(OutgoingMessage::new(&version_response));
        }

        server
            .with_notifier(&mut notifier)
            .handle_synic_message(in_msg(protocol::MessageType::REQUEST_OFFERS, ()))
            .unwrap();

        let channel_id = ChannelId(1);
        notifier.check_messages(&[
            OutgoingMessage::new(&protocol::OfferChannel {
                interface_id,
                instance_id,
                channel_id,
                connection_id: 0x2001,
                is_dedicated: 1,
                monitor_id: 0xff,
                ..protocol::OfferChannel::new_zeroed()
            }),
            OutgoingMessage::new(&protocol::AllOffersDelivered {}),
        ]);

        let open_channel = protocol::OpenChannel {
            channel_id,
            open_id: 1,
            ring_buffer_gpadl_id: GpadlId(1),
            target_vp: 3,
            downstream_ring_buffer_page_offset: 2,
            user_data: UserDefinedData::new_zeroed(),
        };

        let mut event_flag = 1;
        let mut connection_id = 0x2001;
        let mut expected_flags = protocol::OpenChannelFlags::new();
        if version >= Version::Copper
            && (feature_flags.guest_specified_signal_parameters()
                || feature_flags.channel_interrupt_redirection())
        {
            if feature_flags.channel_interrupt_redirection() {
                expected_flags.set_redirect_interrupt(true);
            }

            if feature_flags.guest_specified_signal_parameters() {
                event_flag = 2;
                connection_id = 0x2002;
            }

            server
                .with_notifier(&mut notifier)
                .handle_synic_message(in_msg(
                    protocol::MessageType::OPEN_CHANNEL,
                    protocol::OpenChannel2 {
                        open_channel,
                        event_flag: 2,
                        connection_id: 0x2002,
                        flags: u16::from(
                            protocol::OpenChannelFlags::new().with_redirect_interrupt(true),
                        ) | 0xabc, // a real flag and some junk
                    },
                ))
                .unwrap();
        } else {
            server
                .with_notifier(&mut notifier)
                .handle_synic_message(in_msg(protocol::MessageType::OPEN_CHANNEL, open_channel))
                .unwrap();
        }

        let (id, action) = recv.recv().unwrap();
        assert_eq!(id, offer_id);
        let Action::Open(op, ..) = action else {
            panic!("unexpected action: {:?}", action);
        };
        assert_eq!(op.open_data.ring_gpadl_id, GpadlId(1));
        assert_eq!(op.open_data.ring_offset, 2);
        assert_eq!(op.open_data.target_vp, 3);
        assert_eq!(op.open_data.event_flag, event_flag);
        assert_eq!(op.open_data.connection_id, connection_id);
        assert_eq!(op.connection_id, connection_id);
        assert_eq!(op.event_flag, event_flag);
        assert_eq!(op.monitor_id, None);
        assert_eq!(op.flags, expected_flags);

        server
            .with_notifier(&mut notifier)
            .open_complete(offer_id, 0);

        notifier.check_message(OutgoingMessage::new(&protocol::OpenResult {
            channel_id,
            open_id: 1,
            status: 0,
        }));

        server
            .with_notifier(&mut notifier)
            .handle_synic_message(in_msg(
                protocol::MessageType::MODIFY_CHANNEL,
                protocol::ModifyChannel {
                    channel_id,
                    target_vp: 4,
                },
            ))
            .unwrap();

        let (id, action) = recv.recv().unwrap();
        assert_eq!(id, offer_id);
        assert!(matches!(action, Action::Modify { target_vp: 4 }));

        server
            .with_notifier(&mut notifier)
            .modify_channel_complete(id, 0);

        if version >= Version::Iron {
            notifier.check_message(OutgoingMessage::new(&protocol::ModifyChannelResponse {
                channel_id,
                status: 0,
            }));
        }

        assert!(notifier.messages.is_empty());

        server.with_notifier(&mut notifier).revoke_channel(offer_id);

        server
            .with_notifier(&mut notifier)
            .handle_synic_message(in_msg(
                protocol::MessageType::REL_ID_RELEASED,
                protocol::RelIdReleased { channel_id },
            ))
            .unwrap();
    }

    #[test]
    fn test_hvsock() {
        test_hvsock_helper(Version::Win10, false);
    }

    #[test]
    fn test_hvsock_rs3() {
        test_hvsock_helper(Version::Win10Rs3_0, false);
    }

    #[test]
    fn test_hvsock_rs5() {
        test_hvsock_helper(Version::Win10Rs5, false);
        test_hvsock_helper(Version::Win10Rs5, true);
    }

    fn test_hvsock_helper(version: Version, force_small_message: bool) {
        let (mut notifier, _recv) = TestNotifier::new();
        let mut server = Server::new(Vtl::Vtl0, MESSAGE_CONNECTION_ID, 0);

        server
            .with_notifier(&mut notifier)
            .handle_synic_message(in_msg(
                protocol::MessageType::INITIATE_CONTACT,
                protocol::InitiateContact {
                    version_requested: version as u32,
                    target_message_vp: 0,
                    interrupt_page_or_target_info: 0,
                    parent_to_child_monitor_page_gpa: 0,
                    child_to_parent_monitor_page_gpa: 0,
                },
            ))
            .unwrap();

        let request = notifier.next_action();
        assert_eq!(
            request,
            ModifyConnectionRequest {
                version: Some(version as u32),
                monitor_page: Update::Reset,
                interrupt_page: Update::Reset,
                target_message_vp: Some(0),
                ..Default::default()
            }
        );

        server
            .with_notifier(&mut notifier)
            .complete_initiate_contact(ModifyConnectionResponse::Supported(
                protocol::ConnectionState::SUCCESSFUL,
                FeatureFlags::all(),
            ));

        // Discard the version response message.
        notifier.messages.pop_front();

        let service_id = Guid::new_random();
        let endpoint_id = Guid::new_random();
        let request_msg = if version >= Version::Win10Rs5 && !force_small_message {
            in_msg(
                protocol::MessageType::TL_CONNECT_REQUEST,
                protocol::TlConnectRequest2 {
                    base: protocol::TlConnectRequest {
                        service_id,
                        endpoint_id,
                    },
                    silo_id: Guid::ZERO,
                },
            )
        } else {
            in_msg(
                protocol::MessageType::TL_CONNECT_REQUEST,
                protocol::TlConnectRequest {
                    service_id,
                    endpoint_id,
                },
            )
        };

        server
            .with_notifier(&mut notifier)
            .handle_synic_message(request_msg)
            .unwrap();

        let request = notifier.hvsock_requests.pop().unwrap();
        assert_eq!(request.service_id, service_id);
        assert_eq!(request.endpoint_id, endpoint_id);
        assert!(notifier.hvsock_requests.is_empty());

        // Notify the guest of connection failure.
        server
            .with_notifier(&mut notifier)
            .send_tl_connect_result(HvsockConnectResult::from_request(&request, false));

        if version >= Version::Win10Rs3_0 {
            notifier.check_message(OutgoingMessage::new(&protocol::TlConnectResult {
                service_id: request.service_id,
                endpoint_id: request.endpoint_id,
                status: protocol::STATUS_CONNECTION_REFUSED,
            }));
        }

        assert!(notifier.messages.is_empty());
    }

    struct TestEnv {
        server: Server,
        notifier: TestNotifier,
        version: Option<VersionInfo>,
        _recv: mpsc::Receiver<(OfferId, Action)>,
    }

    impl TestEnv {
        fn new() -> Self {
            let (notifier, _recv) = TestNotifier::new();
            let server = Server::new(Vtl::Vtl0, MESSAGE_CONNECTION_ID, 0);
            Self {
                server,
                notifier,
                version: None,
                _recv,
            }
        }

        fn c(&mut self) -> ServerWithNotifier<'_, TestNotifier> {
            self.server.with_notifier(&mut self.notifier)
        }

        // Completes a reset operation if the server send a modify request as part of it. This
        // shouldn't be called if the server was not connected or had no open channels or gpadls
        // during the reset.
        fn complete_reset(&mut self) {
            let _ = self.next_action();
            self.c()
                .complete_modify_connection(ModifyConnectionResponse::Supported(
                    protocol::ConnectionState::SUCCESSFUL,
                    FeatureFlags::all(),
                ));
        }

        fn offer(&mut self, id: u32) -> OfferId {
            self.offer_inner(id, id, false, None, None, OfferFlags::new())
        }

        fn offer_with_mnf(&mut self, id: u32) -> OfferId {
            self.offer_inner(id, id, true, None, None, OfferFlags::new())
        }

        fn offer_with_preset_mnf(&mut self, id: u32, monitor_id: u8) -> OfferId {
            self.offer_inner(id, id, false, None, Some(monitor_id), OfferFlags::new())
        }

        fn offer_with_order(
            &mut self,
            interface_id: u32,
            instance_id: u32,
            order: Option<u32>,
        ) -> OfferId {
            self.offer_inner(
                interface_id,
                instance_id,
                false,
                order,
                None,
                OfferFlags::new(),
            )
        }

        fn offer_with_flags(&mut self, id: u32, flags: OfferFlags) -> OfferId {
            self.offer_inner(id, id, false, None, None, flags)
        }

        fn offer_inner(
            &mut self,
            interface_id: u32,
            instance_id: u32,
            use_mnf: bool,
            offer_order: Option<u32>,
            monitor_id: Option<u8>,
            flags: OfferFlags,
        ) -> OfferId {
            self.c()
                .offer_channel(OfferParamsInternal {
                    instance_id: Guid {
                        data1: instance_id,
                        ..Guid::ZERO
                    },
                    interface_id: Guid {
                        data1: interface_id,
                        ..Guid::ZERO
                    },
                    use_mnf,
                    offer_order,
                    monitor_id,
                    flags,
                    ..Default::default()
                })
                .unwrap()
        }

        fn open(&mut self, id: u32) {
            self.c()
                .handle_open_channel(&protocol::OpenChannel2 {
                    open_channel: protocol::OpenChannel {
                        channel_id: ChannelId(id),
                        ..FromZeros::new_zeroed()
                    },
                    ..FromZeros::new_zeroed()
                })
                .unwrap()
        }

        fn close(&mut self, id: u32) -> Result<(), ChannelError> {
            self.c().handle_close_channel(&protocol::CloseChannel {
                channel_id: ChannelId(id),
            })
        }

        fn open_reserved(&mut self, id: u32, target_vp: u32, target_sint: u32) {
            let version = self.server.state.get_version().expect("vmbus connected");

            self.c()
                .handle_open_reserved_channel(
                    &protocol::OpenReservedChannel {
                        channel_id: ChannelId(id),
                        target_vp,
                        target_sint,
                        ring_buffer_gpadl: GpadlId(id),
                        ..FromZeros::new_zeroed()
                    },
                    version,
                )
                .unwrap()
        }

        fn close_reserved(&mut self, id: u32, target_vp: u32, target_sint: u32) {
            self.c()
                .handle_close_reserved_channel(&protocol::CloseReservedChannel {
                    channel_id: ChannelId(id),
                    target_vp,
                    target_sint,
                })
                .unwrap();
        }

        fn gpadl(&mut self, channel_id: u32, gpadl_id: u32) {
            self.c()
                .handle_gpadl_header(
                    &protocol::GpadlHeader {
                        channel_id: ChannelId(channel_id),
                        gpadl_id: GpadlId(gpadl_id),
                        count: 1,
                        len: 16,
                    },
                    [1u64, 0u64].as_bytes(),
                )
                .unwrap();
        }

        fn teardown_gpadl(&mut self, channel_id: u32, gpadl_id: u32) {
            self.c()
                .handle_gpadl_teardown(&protocol::GpadlTeardown {
                    channel_id: ChannelId(channel_id),
                    gpadl_id: GpadlId(gpadl_id),
                })
                .unwrap();
        }

        fn release(&mut self, id: u32) {
            self.c()
                .handle_rel_id_released(&protocol::RelIdReleased {
                    channel_id: ChannelId(id),
                })
                .unwrap();
        }

        fn connect(&mut self, version: Version, feature_flags: FeatureFlags) {
            self.start_connect(version, feature_flags, false);
            self.complete_connect();
        }

        fn connect_trusted(&mut self, version: Version, feature_flags: FeatureFlags) {
            self.start_connect(version, feature_flags, true);
            self.complete_connect();
        }

        fn start_connect(&mut self, version: Version, feature_flags: FeatureFlags, trusted: bool) {
            self.version = Some(VersionInfo {
                version,
                feature_flags,
            });

            let result = self.c().handle_synic_message(in_msg_ex(
                protocol::MessageType::INITIATE_CONTACT,
                protocol::InitiateContact2 {
                    initiate_contact: protocol::InitiateContact {
                        version_requested: version as u32,
                        interrupt_page_or_target_info: TargetInfo::new()
                            .with_sint(SINT)
                            .with_vtl(0)
                            .with_feature_flags(feature_flags.into())
                            .into(),
                        child_to_parent_monitor_page_gpa: 0x123f000,
                        parent_to_child_monitor_page_gpa: 0x321f000,
                        ..FromZeros::new_zeroed()
                    },
                    client_id: Guid::ZERO,
                },
                false,
                trusted,
            ));
            assert!(result.is_ok());

            let request = self.notifier.next_action();
            assert_eq!(
                request,
                ModifyConnectionRequest {
                    version: Some(version as u32),
                    monitor_page: Update::Set(MonitorPageGpas {
                        child_to_parent: 0x123f000,
                        parent_to_child: 0x321f000,
                    }),
                    interrupt_page: Update::Reset,
                    target_message_vp: Some(0),
                    ..Default::default()
                }
            );
        }

        fn complete_connect(&mut self) {
            self.c()
                .complete_initiate_contact(ModifyConnectionResponse::Supported(
                    protocol::ConnectionState::SUCCESSFUL,
                    FeatureFlags::all(),
                ));

            let version = self.version.unwrap();
            if version.version >= Version::Copper {
                let response = self.notifier.get_message::<protocol::VersionResponse2>();
                assert_eq!(response.version_response.version_supported, 1);
                self.version = Some(VersionInfo {
                    version: version.version,
                    feature_flags: version.feature_flags & response.supported_features.into(),
                })
            } else {
                let response = self.notifier.get_message::<protocol::VersionResponse>();
                assert_eq!(response.version_supported, 1);
            }
        }

        fn send_message(&mut self, message: SynicMessage) {
            self.try_send_message(message).unwrap();
        }

        fn try_send_message(&mut self, message: SynicMessage) -> Result<(), ChannelError> {
            self.c().handle_synic_message(message)
        }

        fn next_action(&mut self) -> ModifyConnectionRequest {
            self.notifier.next_action()
        }
    }

    /// Ensure that channels can be offered at each stage of connection.
    #[test]
    fn test_hot_add() {
        let mut env = TestEnv::new();
        let offer_id1 = env.offer(1);
        let result = env.c().handle_initiate_contact(
            &protocol::InitiateContact2 {
                initiate_contact: protocol::InitiateContact {
                    version_requested: Version::Win10 as u32,
                    ..FromZeros::new_zeroed()
                },
                ..FromZeros::new_zeroed()
            },
            &SynicMessage::default(),
            true,
        );
        assert!(result.is_ok());
        let offer_id2 = env.offer(2);
        env.c()
            .complete_initiate_contact(ModifyConnectionResponse::Supported(
                protocol::ConnectionState::SUCCESSFUL,
                FeatureFlags::all(),
            ));
        let offer_id3 = env.offer(3);
        env.c().handle_request_offers().unwrap();
        let offer_id4 = env.offer(4);
        env.open(1);
        env.open(2);
        env.open(3);
        env.open(4);
        env.c().open_complete(offer_id1, 0);
        env.c().open_complete(offer_id2, 0);
        env.c().open_complete(offer_id3, 0);
        env.c().open_complete(offer_id4, 0);
        env.c().reset();
        env.c().close_complete(offer_id1);
        env.c().close_complete(offer_id2);
        env.c().close_complete(offer_id3);
        env.c().close_complete(offer_id4);
        env.complete_reset();
        assert!(env.notifier.is_reset());
    }

    #[test]
    fn test_save_restore_with_no_connection() {
        let mut env = TestEnv::new();

        let offer_id1 = env.offer(1);
        let _offer_id2 = env.offer(2);

        let state = env.server.save();
        env.c().reset();
        assert!(env.notifier.is_reset());
        env.server.restore(state).unwrap();
        env.c().restore_channel(offer_id1, false).unwrap();
        env.c().post_restore().unwrap();
    }

    #[test]
    fn test_save_restore_with_connection() {
        let mut env = TestEnv::new();

        let offer_id1 = env.offer_with_mnf(1);
        let offer_id2 = env.offer(2);
        let offer_id3 = env.offer_with_mnf(3);
        let offer_id4 = env.offer(4);
        let offer_id5 = env.offer_with_mnf(5);
        let offer_id6 = env.offer(6);
        let offer_id7 = env.offer(7);
        let offer_id8 = env.offer(8);
        let offer_id9 = env.offer(9);
        let offer_id10 = env.offer(10);

        let expected_monitor = MonitorPageGpas {
            child_to_parent: 0x123f000,
            parent_to_child: 0x321f000,
        };

        env.connect(Version::Win10, FeatureFlags::new());
        assert_eq!(env.notifier.monitor_page, Some(expected_monitor));

        env.c().handle_request_offers().unwrap();
        assert_eq!(env.server.assigned_monitors.bitmap(), 7);

        env.open(1);
        env.open(2);
        env.open(3);
        env.open(5);

        env.c().open_complete(offer_id1, 0);
        env.c().open_complete(offer_id2, 0);
        env.c().open_complete(offer_id5, 0);

        env.gpadl(1, 10);
        env.c().gpadl_create_complete(offer_id1, GpadlId(10), 0);
        env.gpadl(1, 11);
        env.gpadl(2, 20);
        env.c().gpadl_create_complete(offer_id2, GpadlId(20), 0);
        env.gpadl(2, 21);
        env.gpadl(3, 30);
        env.c().gpadl_create_complete(offer_id3, GpadlId(30), 0);
        env.gpadl(3, 31);

        // Test Opening, Open, and Closing save for reserved channels
        env.open_reserved(7, 1, SINT.into());
        env.open_reserved(8, 2, SINT.into());
        env.open_reserved(9, 3, SINT.into());
        env.c().open_complete(offer_id8, 0);
        env.c().open_complete(offer_id9, 0);
        env.close_reserved(9, 3, SINT.into());

        // Revoke an offer but don't have the "guest" release it, so we can then mark it as
        // reoffered.
        env.c().revoke_channel(offer_id10);
        let offer_id10 = env.offer(10);

        let state = env.server.save();

        env.c().reset();

        env.c().close_complete(offer_id1);
        env.c().close_complete(offer_id2);
        env.c().open_complete(offer_id3, -1);
        env.c().close_complete(offer_id5);
        env.c().open_complete(offer_id7, -1);
        env.c().close_complete(offer_id8);
        env.c().close_complete(offer_id9);

        env.c().gpadl_teardown_complete(offer_id1, GpadlId(10));
        env.c().gpadl_create_complete(offer_id1, GpadlId(11), -1);
        env.c().gpadl_teardown_complete(offer_id2, GpadlId(20));
        env.c().gpadl_create_complete(offer_id2, GpadlId(21), -1);
        env.c().gpadl_teardown_complete(offer_id3, GpadlId(30));
        env.c().gpadl_create_complete(offer_id3, GpadlId(31), -1);

        env.complete_reset();
        env.notifier.check_reset();

        env.c().revoke_channel(offer_id5);
        env.c().revoke_channel(offer_id6);

        env.server.restore(state.clone()).unwrap();

        env.c().revoke_channel(offer_id1);
        env.c().revoke_channel(offer_id4);
        env.c().restore_channel(offer_id3, false).unwrap();
        let offer_id5 = env.offer_with_mnf(5);
        env.c().restore_channel(offer_id5, true).unwrap();
        env.c().restore_channel(offer_id7, false).unwrap();
        env.c().restore_channel(offer_id8, true).unwrap();
        env.c().restore_channel(offer_id9, true).unwrap();
        env.c().restore_channel(offer_id10, false).unwrap();
        assert!(matches!(
            env.server.channels[offer_id10].state,
            ChannelState::Reoffered
        ));

        env.c().post_restore().unwrap();

        assert_eq!(env.notifier.monitor_page, Some(expected_monitor));
        assert_eq!(env.notifier.target_message_vp, Some(0));

        assert_eq!(env.server.assigned_monitors.bitmap(), 6);
        env.release(1);
        env.release(2);
        env.release(4);

        // Check reserved channels have been restored to the same state
        env.c().open_complete(offer_id7, 0);
        env.close_reserved(8, 2, SINT.into());
        env.c().close_complete(offer_id8);
        env.c().close_complete(offer_id9);

        env.c().reset();

        env.c().open_complete(offer_id3, -1);
        env.c().gpadl_teardown_complete(offer_id3, GpadlId(30));
        env.c().gpadl_create_complete(offer_id3, GpadlId(31), -1);
        env.c().close_complete(offer_id5);
        env.c().close_complete(offer_id7);

        env.complete_reset();
        env.notifier.check_reset();

        env.server.restore(state).unwrap();
        env.c().restore_channel(offer_id3, false).unwrap();
        env.c().post_restore().unwrap();
        assert_eq!(env.notifier.monitor_page, Some(expected_monitor));
        assert_eq!(env.notifier.target_message_vp, Some(0));
    }

    #[test]
    fn test_save_restore_connecting() {
        let mut env = TestEnv::new();

        let offer_id1 = env.offer_with_mnf(1);
        let _offer_id2 = env.offer(2);

        env.start_connect(Version::Win10, FeatureFlags::new(), false);
        assert_eq!(
            env.notifier.monitor_page,
            Some(MonitorPageGpas {
                child_to_parent: 0x123f000,
                parent_to_child: 0x321f000
            })
        );

        let state = env.server.save();

        env.c().reset();
        // We have to "complete" the connection to let the reset go through.
        env.complete_connect();
        env.notifier.check_reset();

        env.server.restore(state).unwrap();
        env.c().restore_channel(offer_id1, false).unwrap();
        env.c().post_restore().unwrap();
        assert_eq!(
            env.notifier.monitor_page,
            Some(MonitorPageGpas {
                child_to_parent: 0x123f000,
                parent_to_child: 0x321f000
            })
        );

        // Restore should resend the modify connection request.
        let request = env.next_action();
        assert_eq!(
            request,
            ModifyConnectionRequest {
                version: Some(Version::Win10 as u32),
                monitor_page: Update::Set(MonitorPageGpas {
                    child_to_parent: 0x123f000,
                    parent_to_child: 0x321f000,
                }),
                interrupt_page: Update::Reset,
                target_message_vp: Some(0),
                force: true,
                ..Default::default()
            }
        );

        assert_eq!(Some(0), env.notifier.target_message_vp);

        // We can successfully complete connecting after restore.
        env.complete_connect();
    }

    #[test]
    fn test_save_restore_modifying() {
        let mut env = TestEnv::new();
        env.connect(
            Version::Copper,
            FeatureFlags::new().with_modify_connection(true),
        );

        let expected = MonitorPageGpas {
            parent_to_child: 0x123f000,
            child_to_parent: 0x321f000,
        };

        env.send_message(in_msg(
            protocol::MessageType::MODIFY_CONNECTION,
            protocol::ModifyConnection {
                parent_to_child_monitor_page_gpa: expected.parent_to_child,
                child_to_parent_monitor_page_gpa: expected.child_to_parent,
            },
        ));

        // Discard ModifyConnectionRequest
        env.next_action();

        assert_eq!(env.notifier.monitor_page, Some(expected));

        let state = env.server.save();
        env.c().reset();
        env.notifier.check_reset();
        env.server.restore(state).unwrap();
        env.c().post_restore().unwrap();

        // Restore should have resent the request.
        let request = env.next_action();
        assert_eq!(
            request,
            ModifyConnectionRequest {
                monitor_page: Update::Set(MonitorPageGpas {
                    parent_to_child: 0x123f000,
                    child_to_parent: 0x321f000,
                }),
                interrupt_page: Update::Reset,
                target_message_vp: Some(0),
                force: true,
                ..Default::default()
            }
        );

        assert_eq!(env.notifier.monitor_page, Some(expected));

        // We can complete the modify request after restore.
        env.c()
            .complete_modify_connection(ModifyConnectionResponse::Supported(
                protocol::ConnectionState::SUCCESSFUL,
                FeatureFlags::all(),
            ));

        env.notifier
            .check_message(OutgoingMessage::new(&protocol::ModifyConnectionResponse {
                connection_state: protocol::ConnectionState::SUCCESSFUL,
            }));
    }

    #[test]
    fn test_modify_connection() {
        let mut env = TestEnv::new();
        env.connect(
            Version::Copper,
            FeatureFlags::new().with_modify_connection(true),
        );

        env.send_message(in_msg(
            protocol::MessageType::MODIFY_CONNECTION,
            protocol::ModifyConnection {
                parent_to_child_monitor_page_gpa: 5,
                child_to_parent_monitor_page_gpa: 6,
            },
        ));

        assert_eq!(
            env.notifier.monitor_page,
            Some(MonitorPageGpas {
                parent_to_child: 5,
                child_to_parent: 6
            })
        );

        let request = env.next_action();
        assert_eq!(
            request,
            ModifyConnectionRequest {
                monitor_page: Update::Set(MonitorPageGpas {
                    child_to_parent: 6,
                    parent_to_child: 5,
                }),
                ..Default::default()
            }
        );

        env.c()
            .complete_modify_connection(ModifyConnectionResponse::Supported(
                protocol::ConnectionState::FAILED_UNKNOWN_FAILURE,
                FeatureFlags::all(),
            ));

        env.notifier
            .check_message(OutgoingMessage::new(&protocol::ModifyConnectionResponse {
                connection_state: protocol::ConnectionState::FAILED_UNKNOWN_FAILURE,
            }));
    }

    #[test]
    fn test_modify_connection_unsupported() {
        let mut env = TestEnv::new();
        env.connect(Version::Copper, FeatureFlags::new());

        let err = env
            .try_send_message(in_msg(
                protocol::MessageType::MODIFY_CONNECTION,
                protocol::ModifyConnection {
                    parent_to_child_monitor_page_gpa: 5,
                    child_to_parent_monitor_page_gpa: 6,
                },
            ))
            .unwrap_err();

        assert!(matches!(
            err,
            ChannelError::ParseError(protocol::ParseError::InvalidMessageType(
                protocol::MessageType::MODIFY_CONNECTION
            ))
        ));
    }

    #[test]
    fn test_reserved_channels() {
        let mut env = TestEnv::new();

        let offer_id1 = env.offer(1);
        let offer_id2 = env.offer(2);
        let offer_id3 = env.offer(3);

        env.connect(Version::Win10, FeatureFlags::new());
        env.c().handle_request_offers().unwrap();

        // Check gpadl doesn't prevent unload or get torndown on disconnect
        env.gpadl(1, 10);
        env.c().gpadl_create_complete(offer_id1, GpadlId(10), 0);

        env.notifier.messages.clear();

        // Open responses should be sent to the provided target
        env.open_reserved(1, 1, SINT.into());
        env.c().open_complete(offer_id1, 0);
        env.notifier.check_message_with_target(
            OutgoingMessage::new(&protocol::OpenResult {
                channel_id: ChannelId(1),
                ..FromZeros::new_zeroed()
            }),
            MessageTarget::ReservedChannel(offer_id1),
        );
        env.open_reserved(2, 2, SINT.into());
        env.c().open_complete(offer_id2, 0);
        env.open_reserved(3, 3, SINT.into());
        env.c().open_complete(offer_id3, 0);

        // This should fail
        assert!(matches!(env.close(2), Err(ChannelError::ChannelReserved)));

        // Reserved channels and gpadls should stay open across unloads
        env.c().handle_unload();

        // Closing while disconnected should work
        env.close_reserved(2, 2, SINT.into());
        env.c().close_complete(offer_id2);

        env.notifier.messages.clear();
        env.connect(Version::Copper, FeatureFlags::new());
        env.c().handle_request_offers().unwrap();

        // Check reserved gpadl gets torndown on reset
        // Duplicate GPADL IDs across different channels should also work
        env.gpadl(2, 10);
        env.c().gpadl_create_complete(offer_id2, GpadlId(10), 0);

        // Reopening the same offer should work
        env.open_reserved(2, 3, SINT.into());
        env.c().open_complete(offer_id2, 0);

        env.notifier.messages.clear();

        // The channel should still be open after disconnect/reconnect
        // and close responses should be sent to the provided target
        env.close_reserved(1, 4, SINT.into());
        env.c().close_complete(offer_id1);
        assert_eq!(
            env.notifier.reserved_channel_update.take(),
            Some((offer_id1, ConnectionTarget { vp: 4, sint: SINT }))
        );
        env.notifier.check_message_with_target(
            OutgoingMessage::new(&protocol::CloseReservedChannelResponse {
                channel_id: ChannelId(1),
            }),
            MessageTarget::ReservedChannel(offer_id1),
        );
        env.teardown_gpadl(1, 10);
        env.c().gpadl_teardown_complete(offer_id1, GpadlId(10));

        // Reset should force reserved channels closed
        env.c().reset();
        env.c().close_complete(offer_id2);
        env.c().gpadl_teardown_complete(offer_id2, GpadlId(10));
        env.c().close_complete(offer_id3);

        env.complete_reset();
        assert!(env.notifier.is_reset());
    }

    #[test]
    fn test_disconnected_reset() {
        let mut env = TestEnv::new();

        let offer_id1 = env.offer(1);

        env.connect(Version::Win10, FeatureFlags::new());
        env.c().handle_request_offers().unwrap();

        env.gpadl(1, 10);
        env.c().gpadl_create_complete(offer_id1, GpadlId(10), 0);
        env.open_reserved(1, 1, SINT.into());
        env.c().open_complete(offer_id1, 0);

        env.c().handle_unload();

        // Reset while disconnected should cleanup reserved channels
        // and complete disconnect automatically
        env.c().reset();
        env.c().close_complete(offer_id1);
        env.c().gpadl_teardown_complete(offer_id1, GpadlId(10));

        env.complete_reset();
        assert!(env.notifier.is_reset());

        let offer_id2 = env.offer(2);

        env.notifier.messages.clear();
        env.connect(Version::Win10, FeatureFlags::new());
        env.c().handle_request_offers().unwrap();

        env.gpadl(2, 20);
        env.c().gpadl_create_complete(offer_id2, GpadlId(20), 0);
        env.open_reserved(2, 2, SINT.into());
        env.c().open_complete(offer_id2, 0);

        env.c().handle_unload();

        env.close_reserved(2, 2, SINT.into());
        env.c().close_complete(offer_id2);
        env.c().gpadl_teardown_complete(offer_id2, GpadlId(20));

        env.c().reset();
        assert!(env.notifier.is_reset());
    }

    #[test]
    fn test_mnf_channel() {
        let mut env = TestEnv::new();

        // This test combines server-handled and preset MNF IDs, which can't happen normally, but
        // it simplifies the test.
        let _offer_id1 = env.offer(1);
        let _offer_id2 = env.offer_with_mnf(2);
        let _offer_id3 = env.offer_with_preset_mnf(3, 5);

        env.connect(Version::Copper, FeatureFlags::new());
        env.c().handle_request_offers().unwrap();

        // Preset monitor ID should not be in the bitmap.
        assert_eq!(env.server.assigned_monitors.bitmap(), 1);

        env.notifier.check_messages(&[
            OutgoingMessage::new(&protocol::OfferChannel {
                interface_id: Guid {
                    data1: 1,
                    ..Guid::ZERO
                },
                instance_id: Guid {
                    data1: 1,
                    ..Guid::ZERO
                },
                channel_id: ChannelId(1),
                connection_id: 0x2001,
                is_dedicated: 1,
                monitor_id: 0xff,
                ..protocol::OfferChannel::new_zeroed()
            }),
            OutgoingMessage::new(&protocol::OfferChannel {
                interface_id: Guid {
                    data1: 2,
                    ..Guid::ZERO
                },
                instance_id: Guid {
                    data1: 2,
                    ..Guid::ZERO
                },
                channel_id: ChannelId(2),
                connection_id: 0x2002,
                is_dedicated: 1,
                monitor_id: 0,
                monitor_allocated: 1,
                ..protocol::OfferChannel::new_zeroed()
            }),
            OutgoingMessage::new(&protocol::OfferChannel {
                interface_id: Guid {
                    data1: 3,
                    ..Guid::ZERO
                },
                instance_id: Guid {
                    data1: 3,
                    ..Guid::ZERO
                },
                channel_id: ChannelId(3),
                connection_id: 0x2003,
                is_dedicated: 1,
                monitor_id: 5,
                monitor_allocated: 1,
                ..protocol::OfferChannel::new_zeroed()
            }),
            OutgoingMessage::new(&protocol::AllOffersDelivered {}),
        ])
    }

    #[test]
    fn test_channel_id_order() {
        let mut env = TestEnv::new();

        let _offer_id1 = env.offer(3);
        let _offer_id2 = env.offer(10);
        let _offer_id3 = env.offer(5);
        let _offer_id4 = env.offer(17);
        let _offer_id5 = env.offer_with_order(5, 6, Some(2));
        let _offer_id6 = env.offer_with_order(5, 8, Some(1));
        let _offer_id7 = env.offer_with_order(5, 1, None);

        env.connect(Version::Win10, FeatureFlags::new());
        env.c().handle_request_offers().unwrap();

        env.notifier.check_messages(&[
            OutgoingMessage::new(&protocol::OfferChannel {
                interface_id: Guid {
                    data1: 3,
                    ..Guid::ZERO
                },
                instance_id: Guid {
                    data1: 3,
                    ..Guid::ZERO
                },
                channel_id: ChannelId(1),
                connection_id: 0x2001,
                is_dedicated: 1,
                monitor_id: 0xff,
                ..protocol::OfferChannel::new_zeroed()
            }),
            OutgoingMessage::new(&protocol::OfferChannel {
                interface_id: Guid {
                    data1: 5,
                    ..Guid::ZERO
                },
                instance_id: Guid {
                    data1: 8,
                    ..Guid::ZERO
                },
                channel_id: ChannelId(2),
                connection_id: 0x2002,
                is_dedicated: 1,
                monitor_id: 0xff,
                ..protocol::OfferChannel::new_zeroed()
            }),
            OutgoingMessage::new(&protocol::OfferChannel {
                interface_id: Guid {
                    data1: 5,
                    ..Guid::ZERO
                },
                instance_id: Guid {
                    data1: 6,
                    ..Guid::ZERO
                },
                channel_id: ChannelId(3),
                connection_id: 0x2003,
                is_dedicated: 1,
                monitor_id: 0xff,
                ..protocol::OfferChannel::new_zeroed()
            }),
            OutgoingMessage::new(&protocol::OfferChannel {
                interface_id: Guid {
                    data1: 5,
                    ..Guid::ZERO
                },
                instance_id: Guid {
                    data1: 1,
                    ..Guid::ZERO
                },
                channel_id: ChannelId(4),
                connection_id: 0x2004,
                is_dedicated: 1,
                monitor_id: 0xff,
                ..protocol::OfferChannel::new_zeroed()
            }),
            OutgoingMessage::new(&protocol::OfferChannel {
                interface_id: Guid {
                    data1: 5,
                    ..Guid::ZERO
                },
                instance_id: Guid {
                    data1: 5,
                    ..Guid::ZERO
                },
                channel_id: ChannelId(5),
                connection_id: 0x2005,
                is_dedicated: 1,
                monitor_id: 0xff,
                ..protocol::OfferChannel::new_zeroed()
            }),
            OutgoingMessage::new(&protocol::OfferChannel {
                interface_id: Guid {
                    data1: 10,
                    ..Guid::ZERO
                },
                instance_id: Guid {
                    data1: 10,
                    ..Guid::ZERO
                },
                channel_id: ChannelId(6),
                connection_id: 0x2006,
                is_dedicated: 1,
                monitor_id: 0xff,
                ..protocol::OfferChannel::new_zeroed()
            }),
            OutgoingMessage::new(&protocol::OfferChannel {
                interface_id: Guid {
                    data1: 17,
                    ..Guid::ZERO
                },
                instance_id: Guid {
                    data1: 17,
                    ..Guid::ZERO
                },
                channel_id: ChannelId(7),
                connection_id: 0x2007,
                is_dedicated: 1,
                monitor_id: 0xff,
                ..protocol::OfferChannel::new_zeroed()
            }),
            OutgoingMessage::new(&protocol::AllOffersDelivered {}),
        ])
    }

    #[test]
    fn test_confidential_connection() {
        let mut env = TestEnv::new();
        env.connect_trusted(
            Version::Copper,
            FeatureFlags::new().with_confidential_channels(true),
        );

        assert_eq!(
            env.version.unwrap(),
            VersionInfo {
                version: Version::Copper,
                feature_flags: FeatureFlags::new().with_confidential_channels(true)
            }
        );

        env.offer(1); // non-confidential
        env.offer_with_flags(2, OfferFlags::new().with_confidential_ring_buffer(true));
        env.offer_with_flags(
            3,
            OfferFlags::new()
                .with_confidential_ring_buffer(true)
                .with_confidential_external_memory(true),
        );

        // Untrusted messages are rejected when the connection is trusted.
        let error = env
            .try_send_message(in_msg(
                protocol::MessageType::REQUEST_OFFERS,
                protocol::RequestOffers {},
            ))
            .unwrap_err();

        assert!(matches!(error, ChannelError::UntrustedMessage));
        assert!(env.notifier.messages.is_empty());

        // Trusted messages are accepted.
        env.send_message(in_msg_ex(
            protocol::MessageType::REQUEST_OFFERS,
            protocol::RequestOffers {},
            false,
            true,
        ));

        let offer = env.notifier.get_message::<protocol::OfferChannel>();
        assert_eq!(offer.channel_id, ChannelId(1));
        assert_eq!(offer.flags, OfferFlags::new());

        let offer = env.notifier.get_message::<protocol::OfferChannel>();
        assert_eq!(offer.channel_id, ChannelId(2));
        assert_eq!(
            offer.flags,
            OfferFlags::new().with_confidential_ring_buffer(true)
        );

        let offer = env.notifier.get_message::<protocol::OfferChannel>();
        assert_eq!(offer.channel_id, ChannelId(3));
        assert_eq!(
            offer.flags,
            OfferFlags::new()
                .with_confidential_ring_buffer(true)
                .with_confidential_external_memory(true)
        );

        env.notifier
            .check_message(OutgoingMessage::new(&protocol::AllOffersDelivered {}));
    }

    #[test]
    fn test_confidential_channels_unsupported() {
        let mut env = TestEnv::new();

        // A trusted connection without confidential channels is weird, but it makes sure the server
        // looks at the flag, not the trusted state.
        env.connect_trusted(Version::Copper, FeatureFlags::new());

        assert_eq!(
            env.version.unwrap(),
            VersionInfo {
                version: Version::Copper,
                feature_flags: FeatureFlags::new()
            }
        );

        env.offer_with_flags(1, OfferFlags::new().with_enumerate_device_interface(true)); // non-confidential
        env.offer_with_flags(
            2,
            OfferFlags::new()
                .with_named_pipe_mode(true)
                .with_confidential_ring_buffer(true)
                .with_confidential_external_memory(true),
        );

        env.send_message(in_msg_ex(
            protocol::MessageType::REQUEST_OFFERS,
            protocol::RequestOffers {},
            false,
            true,
        ));

        let offer = env.notifier.get_message::<protocol::OfferChannel>();
        assert_eq!(offer.channel_id, ChannelId(1));
        assert_eq!(
            offer.flags,
            OfferFlags::new().with_enumerate_device_interface(true)
        );

        // The confidential channel flags are not sent without the feature flag.
        let offer = env.notifier.get_message::<protocol::OfferChannel>();
        assert_eq!(offer.channel_id, ChannelId(2));
        assert_eq!(offer.flags, OfferFlags::new().with_named_pipe_mode(true));

        env.notifier
            .check_message(OutgoingMessage::new(&protocol::AllOffersDelivered {}));
    }

    #[test]
    fn test_confidential_channels_untrusted() {
        let mut env = TestEnv::new();

        env.connect(
            Version::Copper,
            FeatureFlags::new().with_confidential_channels(true),
        );

        // The server should not offer confidential channel support to untrusted clients, even if
        // requested.
        assert_eq!(
            env.version.unwrap(),
            VersionInfo {
                version: Version::Copper,
                feature_flags: FeatureFlags::new()
            }
        );
    }
}
