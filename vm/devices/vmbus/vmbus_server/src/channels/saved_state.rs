// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::MnfUsage;
use super::Notifier;
use super::OfferError;
use super::OfferParamsInternal;
use super::OfferedInfo;
use super::RestoreState;
use super::SUPPORTED_FEATURE_FLAGS;
use guid::Guid;
pub use inner::SavedState;
use mesh::payload::Protobuf;
use std::fmt::Display;
use thiserror::Error;
use vmbus_channel::bus::OfferKey;
use vmbus_core::protocol::ChannelId;
use vmbus_core::protocol::FeatureFlags;
use vmbus_core::protocol::GpadlId;
use vmbus_core::protocol::Version;
use vmbus_ring::gparange;
use vmcore::monitor::MonitorId;

impl super::Server {
    fn restore_one_channel(&mut self, saved_channel: Channel) -> Result<(), RestoreError> {
        let (info, stub_offer, state) = saved_channel.restore()?;
        if let Some((offer_id, channel)) = self.channels.get_by_key_mut(&saved_channel.key) {
            // There is an existing channel. Restore on top of it.

            if !matches!(channel.state, super::ChannelState::ClientReleased)
                || channel.restore_state != RestoreState::New
            {
                return Err(RestoreError::AlreadyRestored(saved_channel.key));
            }

            // The channel's monitor ID can be already set if it was set by the device, which is
            // the case with relay channels. In that case, it must match the saved ID.
            if let MnfUsage::Relayed { monitor_id } = channel.offer.use_mnf {
                if info.monitor_id != Some(MonitorId(monitor_id)) {
                    return Err(RestoreError::MismatchedMonitorId(
                        monitor_id,
                        saved_channel.monitor_id,
                    ));
                }
            }

            self.assigned_channels
                .set(info.channel_id)?
                .insert(offer_id);

            channel.state = state;
            channel.restore_state = RestoreState::Restoring;
            channel.info = Some(info);
        } else {
            // There is no existing channel.

            let entry = self
                .assigned_channels
                .set(ChannelId(saved_channel.channel_id))?;

            let channel = super::Channel {
                info: Some(info),
                offer: stub_offer,
                state,
                restore_state: RestoreState::Unmatched,
            };

            let offer_id = self.channels.offer(channel);
            entry.insert(offer_id);
        }
        Ok(())
    }

    fn restore_one_gpadl(&mut self, saved_gpadl: Gpadl) -> Result<(), RestoreError> {
        let gpadl_id = GpadlId(saved_gpadl.id);
        let channel_id = ChannelId(saved_gpadl.channel_id);
        let (offer_id, channel) = self
            .channels
            .get_by_channel_id(&self.assigned_channels, channel_id)
            .map_err(|_| RestoreError::MissingGpadlChannel(gpadl_id, channel_id))?;

        if channel.restore_state == RestoreState::New || channel.state.is_released() {
            return Err(RestoreError::MissingGpadlChannel(gpadl_id, channel_id));
        }

        let gpadl = saved_gpadl.restore(channel)?;
        let state = gpadl.state;
        if self.gpadls.insert((gpadl_id, offer_id), gpadl).is_some() {
            return Err(RestoreError::GpadlIdInUse(gpadl_id, channel_id));
        }

        if state == super::GpadlState::InProgress
            && self.incomplete_gpadls.insert(gpadl_id, offer_id).is_some()
        {
            unreachable!("gpadl ID validated above");
        }

        Ok(())
    }

    /// Saves state.
    pub fn save(&self) -> SavedState {
        SavedStateData {
            state: if let Some(state) = self.save_connected_state() {
                SavedConnectionState::Connected(state)
            } else {
                SavedConnectionState::Disconnected(self.save_disconnected_state())
            },
            pending_messages: self.save_pending_messages(),
        }
        .into()
    }

    fn save_connected_state(&self) -> Option<ConnectedState> {
        let connection = Connection::save(&self.state)?;
        let channels = self
            .channels
            .iter()
            .filter_map(|(_, channel)| Channel::save(channel))
            .collect();

        let gpadls = self.save_gpadls();
        Some(ConnectedState {
            connection,
            channels,
            gpadls,
        })
    }

    fn save_gpadls(&self) -> Vec<Gpadl> {
        self.gpadls
            .iter()
            .filter_map(|((gpadl_id, offer_id), gpadl)| {
                Gpadl::save(*gpadl_id, self.channels[*offer_id].info?.channel_id, gpadl)
            })
            .collect()
    }

    fn save_disconnected_state(&self) -> DisconnectedState {
        // Save reserved channels only.
        let channels = self
            .channels
            .iter()
            .filter_map(|(_, channel)| {
                channel
                    .state
                    .is_reserved()
                    .then(|| Channel::save(channel))
                    .flatten()
            })
            .collect();

        // Save the GPADLs for reserved channels.
        // N.B. There cannot be any other GPADLs while disconnected.
        let gpadls = self.save_gpadls();
        DisconnectedState {
            reserved_channels: channels,
            reserved_gpadls: gpadls,
        }
    }

    fn save_pending_messages(&self) -> Vec<OutgoingMessage> {
        self.pending_messages
            .0
            .iter()
            .map(OutgoingMessage::save)
            .collect()
    }
}

impl<'a, N: 'a + Notifier> super::ServerWithNotifier<'a, N> {
    /// Restores state.
    ///
    /// This may be called before or after channels have been offered. After
    /// calling this routine, [`restore_channel`] should be
    /// called for each channel to be restored, possibly interleaved with
    /// additional calls to offer or revoke channels.
    ///
    /// Once all channels are in the appropriate state,
    /// [`revoke_unclaimed_channels`] should be called. This will revoke
    /// any channels that were in the saved state but were not restored via
    /// [`restore_channel`].
    ///
    /// [`revoke_unclaimed_channels`]: super::ServerWithNotifier::revoke_unclaimed_channels
    /// [`restore_channel`]: super::ServerWithNotifier::restore_channel
    pub fn restore(&mut self, saved: SavedState) -> Result<(), RestoreError> {
        tracing::trace!(?saved, "restoring channel state");

        let saved = SavedStateData::from(saved);
        match saved.state {
            SavedConnectionState::Connected(saved) => {
                self.inner.state = saved.connection.restore()?;

                // Restore server state, and resend server notifications if needed. If these notifications
                // were processed before the save, it's harmless as the values will be the same.
                let request = match self.inner.state {
                    super::ConnectionState::Connecting {
                        info,
                        next_action: _,
                    } => Some(super::ModifyConnectionRequest {
                        version: Some(info.version),
                        interrupt_page: info.interrupt_page.into(),
                        monitor_page: info.monitor_page.map(|mp| mp.gpas).into(),
                        target_message_vp: Some(info.target_message_vp),
                        notify_relay: true,
                    }),
                    super::ConnectionState::Connected(info) => {
                        Some(super::ModifyConnectionRequest {
                            version: None,
                            monitor_page: info.monitor_page.map(|mp| mp.gpas).into(),
                            interrupt_page: info.interrupt_page.into(),
                            target_message_vp: Some(info.target_message_vp),
                            // If the save didn't happen while modifying, the relay doesn't need to be notified
                            // of this info as it doesn't constitute a change, we're just restoring existing
                            // connection state.
                            notify_relay: info.modifying,
                        })
                    }
                    // No action needed for these states; if disconnecting, check_disconnected will resend
                    // the reset request if needed.
                    super::ConnectionState::Disconnected
                    | super::ConnectionState::Disconnecting { .. } => None,
                };

                if let Some(request) = request {
                    self.notifier.modify_connection(request)?;
                }

                for saved_channel in saved.channels {
                    self.inner.restore_one_channel(saved_channel)?;
                }

                for saved_gpadl in saved.gpadls {
                    self.inner.restore_one_gpadl(saved_gpadl)?;
                }
            }
            SavedConnectionState::Disconnected(saved) => {
                self.inner.state = super::ConnectionState::Disconnected;
                for saved_channel in saved.reserved_channels {
                    self.inner.restore_one_channel(saved_channel)?;
                }

                for saved_gpadl in saved.reserved_gpadls {
                    self.inner.restore_one_gpadl(saved_gpadl)?;
                }
            }
        }

        self.inner
            .pending_messages
            .0
            .reserve(saved.pending_messages.len());

        for message in saved.pending_messages {
            self.inner.pending_messages.0.push_back(message.restore()?);
        }

        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum RestoreError {
    #[error(transparent)]
    Offer(#[from] OfferError),

    #[error("channel {0} has already been restored")]
    AlreadyRestored(OfferKey),

    #[error("gpadl {} is for missing channel {}", (.0).0, (.1).0)]
    MissingGpadlChannel(GpadlId, ChannelId),

    #[error("gpadl {} is for revoked channel {}", (.0).0, (.1).0)]
    GpadlForRevokedChannel(GpadlId, ChannelId),

    #[error("gpadl {} is already restored", (.0).0)]
    GpadlIdInUse(GpadlId, ChannelId),

    #[error("unsupported protocol version {0:#x}")]
    UnsupportedVersion(u32),

    #[error("invalid gpadl")]
    InvalidGpadl(#[from] gparange::Error),

    #[error("unsupported feature flags {0:#x}")]
    UnsupportedFeatureFlags(u32),

    #[error("channel {0} has a mismatched open state")]
    MismatchedOpenState(OfferKey),

    #[error("channel {0} is missing from the saved state")]
    MissingChannel(OfferKey),

    #[error("unsupported reserved channel protocol version {0:#x}")]
    UnsupportedReserveVersion(u32),

    #[error("unsupported reserved channel feature flags {0:#x}")]
    UnsupportedReserveFeatureFlags(u32),

    #[error("mismatched monitor id; expected {0}, actual {1:?}")]
    MismatchedMonitorId(u8, Option<u8>),

    #[error("monitor ID used by multiple channels in the saved state")]
    DuplicateMonitorId(u8),

    #[error(transparent)]
    ServerError(#[from] anyhow::Error),

    #[error(
        "reserved channel with ID {0} has a pending message but is missing from the saved state"
    )]
    MissingReservedChannel(u32),
    #[error("a saved pending message is larger than the maximum message size")]
    MessageTooLarge,
}

mod inner {
    use super::*;

    /// The top-level saved state for the VMBus channels library. It is placed in its own module to
    /// keep the internals private, and the only thing you can do with it is convert to/from
    /// `SavedStateData`. This enforces that users always consider both the connected and
    /// disconnected states.
    #[derive(Debug, Protobuf, Clone)]
    #[mesh(package = "vmbus.server.channels")]
    pub struct SavedState {
        #[mesh(1)]
        state: Option<ConnectedState>,
        // Disconnected state is used to save any open reserved channels while the guest is
        // disconnected. It is mutually exclusive with `state`, but is separate to maintain saved
        // state compatibility.
        // N.B. In a saved state created by the current version, either state or disconnected_state
        //      is always `Some`, but for older versions, it is possible that both are `None`. They
        //      can never both be `Some`.
        #[mesh(2)]
        disconnected_state: Option<DisconnectedState>,
        #[mesh(3)]
        pending_messages: Vec<OutgoingMessage>,
    }

    impl From<SavedStateData> for SavedState {
        fn from(value: SavedStateData) -> Self {
            let (state, disconnected_state) = match value.state {
                SavedConnectionState::Connected(connected) => (Some(connected), None),
                SavedConnectionState::Disconnected(disconnected) => (None, Some(disconnected)),
            };

            Self {
                state,
                disconnected_state,
                pending_messages: value.pending_messages,
            }
        }
    }

    impl From<SavedState> for SavedStateData {
        fn from(value: SavedState) -> Self {
            Self {
                state: if let Some(connected) = value.state {
                    SavedConnectionState::Connected(connected)
                } else {
                    // Older saved state versions may not have a disconnected state, in which case
                    // we use an empty value which has no channels or gpadls.
                    SavedConnectionState::Disconnected(value.disconnected_state.unwrap_or_default())
                },
                pending_messages: value.pending_messages,
            }
        }
    }
}

/// Represents either connected or disconnected saved state.
#[derive(Debug, Clone)]
pub enum SavedConnectionState {
    Connected(ConnectedState),
    Disconnected(DisconnectedState),
}

/// Alternative representation of the saved state that ensures that all code paths deal with either
/// the connected or disconnected state, and cannot neglect one.
#[derive(Debug, Clone)]
pub struct SavedStateData {
    pub state: SavedConnectionState,
    pub pending_messages: Vec<OutgoingMessage>,
}

impl SavedStateData {
    /// Finds a channel in the saved state.
    pub fn find_channel(&self, offer: OfferKey) -> Option<&Channel> {
        let (channels, _) = self.channels_and_gpadls();
        channels.iter().find(|c| c.key == offer)
    }

    /// Retrieves all the channels and GPADLs from the saved state.
    /// If disconnected, returns any reserved channels and their GPADLs.
    pub fn channels_and_gpadls(&self) -> (&[Channel], &[Gpadl]) {
        match &self.state {
            SavedConnectionState::Connected(connected) => (&connected.channels, &connected.gpadls),
            SavedConnectionState::Disconnected(disconnected) => (
                &disconnected.reserved_channels,
                &disconnected.reserved_gpadls,
            ),
        }
    }
}

#[derive(Debug, Clone, Protobuf)]
#[mesh(package = "vmbus.server.channels")]
pub struct ConnectedState {
    #[mesh(1)]
    pub connection: Connection,
    #[mesh(2)]
    pub channels: Vec<Channel>,
    #[mesh(3)]
    pub gpadls: Vec<Gpadl>,
}

#[derive(Default, Debug, Clone, Protobuf)]
#[mesh(package = "vmbus.server.channels")]
pub struct DisconnectedState {
    #[mesh(1)]
    pub reserved_channels: Vec<Channel>,
    #[mesh(2)]
    pub reserved_gpadls: Vec<Gpadl>,
}

#[derive(Debug, PartialEq, Eq, Clone, Protobuf)]
#[mesh(package = "vmbus.server.channels")]
pub struct VersionInfo {
    #[mesh(1)]
    pub version: u32,
    #[mesh(2)]
    pub feature_flags: u32,
}

impl VersionInfo {
    fn save(value: &super::VersionInfo) -> Self {
        Self {
            version: value.version as u32,
            feature_flags: value.feature_flags.into(),
        }
    }

    fn restore(self, trusted: bool) -> Result<vmbus_core::VersionInfo, RestoreError> {
        let version = super::SUPPORTED_VERSIONS
            .iter()
            .find(|v| self.version == **v as u32)
            .copied()
            .ok_or(RestoreError::UnsupportedVersion(self.version))?;

        let feature_flags = FeatureFlags::from(self.feature_flags);
        let supported_flags = SUPPORTED_FEATURE_FLAGS.with_confidential_channels(trusted);
        if !supported_flags.contains(feature_flags) {
            return Err(RestoreError::UnsupportedFeatureFlags(feature_flags.into()));
        }

        Ok(super::VersionInfo {
            version,
            feature_flags,
        })
    }
}

#[derive(Debug, Clone, Protobuf)]
#[mesh(package = "vmbus.server.channels")]
pub enum Connection {
    #[mesh(1)]
    Disconnecting {
        #[mesh(1)]
        next_action: ConnectionAction,
    },
    #[mesh(2)]
    Connecting {
        #[mesh(1)]
        version: VersionInfo,
        #[mesh(2)]
        interrupt_page: Option<u64>,
        #[mesh(3)]
        monitor_page: Option<MonitorPageGpas>,
        #[mesh(4)]
        target_message_vp: u32,
        #[mesh(5)]
        next_action: ConnectionAction,
        #[mesh(6)]
        client_id: Option<Guid>,
        #[mesh(7)]
        trusted: bool,
    },
    #[mesh(3)]
    Connected {
        #[mesh(1)]
        version: VersionInfo,
        #[mesh(2)]
        offers_sent: bool,
        #[mesh(3)]
        interrupt_page: Option<u64>,
        #[mesh(4)]
        monitor_page: Option<MonitorPageGpas>,
        #[mesh(5)]
        target_message_vp: u32,
        #[mesh(6)]
        modifying: bool,
        #[mesh(7)]
        client_id: Option<Guid>,
        #[mesh(8)]
        trusted: bool,
        #[mesh(9)]
        paused: bool,
    },
}

impl Connection {
    fn save(value: &super::ConnectionState) -> Option<Self> {
        match value {
            super::ConnectionState::Disconnected => {
                // No state to save.
                None
            }
            super::ConnectionState::Connecting { info, next_action } => {
                Some(Connection::Connecting {
                    version: VersionInfo::save(&info.version),
                    interrupt_page: info.interrupt_page,
                    monitor_page: info.monitor_page.map(MonitorPageGpas::save),
                    target_message_vp: info.target_message_vp,
                    next_action: ConnectionAction::save(next_action),
                    client_id: Some(info.client_id),
                    trusted: info.trusted,
                })
            }
            super::ConnectionState::Connected(info) => Some(Connection::Connected {
                version: VersionInfo::save(&info.version),
                offers_sent: info.offers_sent,
                interrupt_page: info.interrupt_page,
                monitor_page: info.monitor_page.map(MonitorPageGpas::save),
                target_message_vp: info.target_message_vp,
                modifying: info.modifying,
                client_id: Some(info.client_id),
                trusted: info.trusted,
                paused: info.paused,
            }),
            super::ConnectionState::Disconnecting {
                next_action,
                modify_sent: _,
            } => Some(Connection::Disconnecting {
                next_action: ConnectionAction::save(next_action),
            }),
        }
    }

    fn restore(self) -> Result<super::ConnectionState, RestoreError> {
        Ok(match self {
            Connection::Connecting {
                version,
                interrupt_page,
                monitor_page,
                target_message_vp,
                next_action,
                client_id,
                trusted,
            } => super::ConnectionState::Connecting {
                info: super::ConnectionInfo {
                    version: version.restore(trusted)?,
                    trusted,
                    interrupt_page,
                    monitor_page: monitor_page.map(MonitorPageGpas::restore),
                    target_message_vp,
                    offers_sent: false,
                    modifying: false,
                    client_id: client_id.unwrap_or(Guid::ZERO),
                    paused: false,
                },
                next_action: next_action.restore(),
            },
            Connection::Connected {
                version,
                offers_sent,
                interrupt_page,
                monitor_page,
                target_message_vp,
                modifying,
                client_id,
                trusted,
                paused,
            } => super::ConnectionState::Connected(super::ConnectionInfo {
                version: version.restore(trusted)?,
                trusted,
                offers_sent,
                interrupt_page,
                monitor_page: monitor_page.map(MonitorPageGpas::restore),
                target_message_vp,
                modifying,
                client_id: client_id.unwrap_or(Guid::ZERO),
                paused,
            }),
            Connection::Disconnecting { next_action } => super::ConnectionState::Disconnecting {
                next_action: next_action.restore(),
                // If the modify request was sent, it will be resent.
                modify_sent: false,
            },
        })
    }
}

#[derive(Debug, Clone, Protobuf)]
#[mesh(package = "vmbus.server.channels")]
pub enum ConnectionAction {
    #[mesh(1)]
    None,
    #[mesh(2)]
    SendUnloadComplete,
    #[mesh(3)]
    Reconnect {
        #[mesh(1)]
        initiate_contact: InitiateContactRequest,
    },
    #[mesh(4)]
    SendFailedVersionResponse,
}

impl ConnectionAction {
    fn save(value: &super::ConnectionAction) -> Self {
        match value {
            super::ConnectionAction::Reset | super::ConnectionAction::None => {
                // The caller is responsible for remembering that a
                // reset was in progress and reissuing it.
                Self::None
            }
            super::ConnectionAction::SendUnloadComplete => Self::SendUnloadComplete,
            super::ConnectionAction::Reconnect { initiate_contact } => Self::Reconnect {
                initiate_contact: InitiateContactRequest::save(initiate_contact),
            },
            super::ConnectionAction::SendFailedVersionResponse => Self::SendFailedVersionResponse,
        }
    }

    fn restore(self) -> super::ConnectionAction {
        match self {
            Self::None => super::ConnectionAction::None,
            Self::SendUnloadComplete => super::ConnectionAction::SendUnloadComplete,
            Self::Reconnect { initiate_contact } => super::ConnectionAction::Reconnect {
                initiate_contact: initiate_contact.restore(),
            },
            Self::SendFailedVersionResponse => super::ConnectionAction::SendFailedVersionResponse,
        }
    }
}

#[derive(Debug, Clone, Protobuf)]
#[mesh(package = "vmbus.server.channels")]
pub struct Channel {
    #[mesh(1)]
    pub key: OfferKey,
    #[mesh(2)]
    pub channel_id: u32,
    #[mesh(3)]
    pub offered_connection_id: u32,
    #[mesh(4)]
    pub state: ChannelState,
    #[mesh(5)]
    pub monitor_id: Option<u8>,
}

impl Channel {
    fn save(value: &super::Channel) -> Option<Self> {
        let info = value.info.as_ref()?;
        let key = value.offer.key();
        if let Some(state) = ChannelState::save(&value.state) {
            tracing::trace!(%key, %state, "channel saved");
            Some(Channel {
                channel_id: info.channel_id.0,
                offered_connection_id: info.connection_id,
                key,
                state,
                monitor_id: info.monitor_id.map(|id| id.0),
            })
        } else {
            tracing::info!(%key, state = %value.state, "skipping channel save");
            None
        }
    }

    fn restore(
        &self,
    ) -> Result<(OfferedInfo, OfferParamsInternal, super::ChannelState), RestoreError> {
        let info = OfferedInfo {
            channel_id: ChannelId(self.channel_id),
            connection_id: self.offered_connection_id,
            monitor_id: self.monitor_id.map(MonitorId),
        };

        let stub_offer = OfferParamsInternal {
            instance_id: self.key.instance_id,
            interface_id: self.key.interface_id,
            subchannel_index: self.key.subchannel_index,
            ..Default::default()
        };

        let state = self.state.restore()?;
        tracing::info!(key = %self.key, %state, "channel restored");
        Ok((info, stub_offer, state))
    }

    pub fn channel_id(&self) -> u32 {
        self.channel_id
    }

    pub fn key(&self) -> OfferKey {
        self.key
    }

    pub fn open_request(&self) -> Option<OpenRequest> {
        match self.state {
            ChannelState::Closed => None,
            ChannelState::Opening { request, .. } => Some(request),
            ChannelState::Open { params, .. } => Some(params),
            ChannelState::Closing { params, .. } => Some(params),
            ChannelState::ClosingReopen { params, .. } => Some(params),
            ChannelState::Revoked => None,
        }
    }
}

#[derive(PartialEq, Eq, Debug, Copy, Clone, Protobuf)]
#[mesh(package = "vmbus.server.channels")]
pub struct InitiateContactRequest {
    #[mesh(1)]
    pub version_requested: u32,
    #[mesh(2)]
    pub target_message_vp: u32,
    #[mesh(3)]
    pub monitor_page: MonitorPageRequest,
    #[mesh(4)]
    pub target_sint: u8,
    #[mesh(5)]
    pub target_vtl: u8,
    #[mesh(6)]
    pub feature_flags: u32,
    #[mesh(7)]
    pub interrupt_page: Option<u64>,
    #[mesh(8)]
    pub client_id: Guid,
    #[mesh(9)]
    pub trusted: bool,
}

impl InitiateContactRequest {
    fn save(value: &super::InitiateContactRequest) -> Self {
        Self {
            version_requested: value.version_requested,
            target_message_vp: value.target_message_vp,
            monitor_page: MonitorPageRequest::save(value.monitor_page),
            target_sint: value.target_sint,
            target_vtl: value.target_vtl,
            feature_flags: value.feature_flags,
            interrupt_page: value.interrupt_page,
            client_id: value.client_id,
            trusted: value.trusted,
        }
    }

    fn restore(self) -> super::InitiateContactRequest {
        super::InitiateContactRequest {
            version_requested: self.version_requested,
            target_message_vp: self.target_message_vp,
            monitor_page: self.monitor_page.restore(),
            target_sint: self.target_sint,
            target_vtl: self.target_vtl,
            feature_flags: self.feature_flags,
            interrupt_page: self.interrupt_page,
            client_id: self.client_id,
            trusted: self.trusted,
        }
    }
}

#[derive(PartialEq, Eq, Debug, Copy, Clone, Protobuf)]
#[mesh(package = "vmbus.server.channels")]
pub struct MonitorPageGpas {
    #[mesh(1)]
    pub parent_to_child: u64,
    #[mesh(2)]
    pub child_to_parent: u64,
}

impl MonitorPageGpas {
    fn save(value: super::MonitorPageGpaInfo) -> Self {
        assert!(
            !value.server_allocated,
            "cannot save with server-allocated monitor pages"
        );
        Self {
            child_to_parent: value.gpas.child_to_parent,
            parent_to_child: value.gpas.parent_to_child,
        }
    }

    fn restore(self) -> super::MonitorPageGpaInfo {
        super::MonitorPageGpaInfo::from_guest_gpas(super::MonitorPageGpas {
            child_to_parent: self.child_to_parent,
            parent_to_child: self.parent_to_child,
        })
    }
}

#[derive(PartialEq, Eq, Debug, Copy, Clone, Protobuf)]
#[mesh(package = "vmbus.server.channels")]
pub enum MonitorPageRequest {
    #[mesh(1)]
    None,
    #[mesh(2)]
    Some(#[mesh(1)] MonitorPageGpas),
    #[mesh(3)]
    Invalid,
}

impl MonitorPageRequest {
    fn save(value: super::MonitorPageRequest) -> Self {
        match value {
            super::MonitorPageRequest::None => MonitorPageRequest::None,
            super::MonitorPageRequest::Some(mp) => MonitorPageRequest::Some(MonitorPageGpas::save(
                super::MonitorPageGpaInfo::from_guest_gpas(mp),
            )),
            super::MonitorPageRequest::Invalid => MonitorPageRequest::Invalid,
        }
    }

    fn restore(self) -> super::MonitorPageRequest {
        match self {
            MonitorPageRequest::None => super::MonitorPageRequest::None,
            MonitorPageRequest::Some(mp) => super::MonitorPageRequest::Some(mp.restore().gpas),
            MonitorPageRequest::Invalid => super::MonitorPageRequest::Invalid,
        }
    }
}

#[derive(PartialEq, Eq, Debug, Copy, Clone, Protobuf)]
#[mesh(package = "vmbus.server.channels")]
pub struct SignalInfo {
    #[mesh(1)]
    pub event_flag: u16,
    #[mesh(2)]
    pub connection_id: u32,
}

impl SignalInfo {
    fn save(value: &super::SignalInfo) -> Self {
        Self {
            event_flag: value.event_flag,
            connection_id: value.connection_id,
        }
    }

    fn restore(self) -> super::SignalInfo {
        super::SignalInfo {
            event_flag: self.event_flag,
            connection_id: self.connection_id,
        }
    }
}

#[derive(Copy, PartialEq, Eq, Clone, Protobuf)]
#[mesh(package = "vmbus.server.channels")]
pub struct OpenRequest {
    #[mesh(1)]
    pub open_id: u32,
    #[mesh(2)]
    pub ring_buffer_gpadl_id: GpadlId,
    #[mesh(3)]
    pub target_vp: u32,
    #[mesh(4)]
    pub downstream_ring_buffer_page_offset: u32,
    #[mesh(5)]
    pub user_data: [u8; 120],
    #[mesh(6)]
    pub guest_specified_interrupt_info: Option<SignalInfo>,
    #[mesh(7)]
    pub flags: u16,
}

impl std::fmt::Debug for OpenRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            open_id,
            ring_buffer_gpadl_id,
            target_vp,
            downstream_ring_buffer_page_offset,
            user_data,
            guest_specified_interrupt_info,
            flags,
        } = self;

        let user_data_display: &dyn std::fmt::Debug = if self.user_data.iter().all(|&b| b == 0) {
            &"[<all-zeroes>]"
        } else {
            struct HexDisplay<'a>(&'a [u8; 120]);
            impl std::fmt::Debug for HexDisplay<'_> {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    write!(f, "[")?;
                    for byte in self.0 {
                        write!(f, "{:02X}", byte)?;
                    }
                    write!(f, "]")
                }
            }
            &HexDisplay(user_data)
        };

        f.debug_struct("OpenRequest")
            .field("open_id", open_id)
            .field("ring_buffer_gpadl_id", ring_buffer_gpadl_id)
            .field("target_vp", target_vp)
            .field(
                "downstream_ring_buffer_page_offset",
                downstream_ring_buffer_page_offset,
            )
            .field("user_data", user_data_display)
            .field(
                "guest_specified_interrupt_info",
                guest_specified_interrupt_info,
            )
            .field("flags", flags)
            .finish()
    }
}

impl OpenRequest {
    fn save(value: &super::OpenRequest) -> Self {
        Self {
            open_id: value.open_id,
            ring_buffer_gpadl_id: value.ring_buffer_gpadl_id,
            target_vp: value.target_vp,
            downstream_ring_buffer_page_offset: value.downstream_ring_buffer_page_offset,
            user_data: value.user_data.into(),
            guest_specified_interrupt_info: value
                .guest_specified_interrupt_info
                .as_ref()
                .map(SignalInfo::save),
            flags: value.flags.into(),
        }
    }

    fn restore(self) -> super::OpenRequest {
        super::OpenRequest {
            open_id: self.open_id,
            ring_buffer_gpadl_id: self.ring_buffer_gpadl_id,
            target_vp: self.target_vp,
            downstream_ring_buffer_page_offset: self.downstream_ring_buffer_page_offset,
            user_data: self.user_data.into(),
            guest_specified_interrupt_info: self
                .guest_specified_interrupt_info
                .map(SignalInfo::restore),
            flags: self.flags.into(),
        }
    }
}

#[derive(Debug, Copy, Clone, Protobuf)]
#[mesh(package = "vmbus.server.channels")]
pub enum ModifyState {
    #[mesh(1)]
    NotModifying,
    #[mesh(2)]
    Modifying {
        #[mesh(1)]
        pending_target_vp: Option<u32>,
    },
}

impl ModifyState {
    fn save(value: &super::ModifyState) -> Self {
        match value {
            super::ModifyState::NotModifying => Self::NotModifying,
            super::ModifyState::Modifying { pending_target_vp } => Self::Modifying {
                pending_target_vp: *pending_target_vp,
            },
        }
    }

    fn restore(self) -> super::ModifyState {
        match self {
            ModifyState::NotModifying => super::ModifyState::NotModifying,
            ModifyState::Modifying { pending_target_vp } => {
                super::ModifyState::Modifying { pending_target_vp }
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Protobuf)]
#[mesh(package = "vmbus.server.channels")]
pub struct ReservedState {
    #[mesh(1)]
    pub version: VersionInfo,
    #[mesh(2)]
    pub vp: u32,
    #[mesh(3)]
    pub sint: u8,
}

impl ReservedState {
    fn save(reserved_state: &super::ReservedState) -> Self {
        Self {
            version: VersionInfo::save(&reserved_state.version),
            vp: reserved_state.target.vp,
            sint: reserved_state.target.sint,
        }
    }

    fn restore(&self) -> Result<super::ReservedState, RestoreError> {
        // We don't know if the connection when the channel was reserved was trusted, so assume it
        // was for what feature flags are accepted here; it doesn't affect any actual behavior.
        let version = self.version.clone().restore(true).map_err(|e| match e {
            RestoreError::UnsupportedVersion(v) => RestoreError::UnsupportedReserveVersion(v),
            RestoreError::UnsupportedFeatureFlags(f) => {
                RestoreError::UnsupportedReserveFeatureFlags(f)
            }
            err => err,
        })?;

        if version.version < Version::Win10 {
            return Err(RestoreError::UnsupportedReserveVersion(
                version.version as u32,
            ));
        }

        Ok(super::ReservedState {
            version,
            target: super::ConnectionTarget {
                vp: self.vp,
                sint: self.sint,
            },
        })
    }
}

#[derive(Debug, Clone, Protobuf)]
#[mesh(package = "vmbus.server.channels")]
pub enum ChannelState {
    #[mesh(1)]
    Closed,
    #[mesh(2)]
    Opening {
        #[mesh(1)]
        request: OpenRequest,
        #[mesh(2)]
        reserved_state: Option<ReservedState>,
    },
    #[mesh(3)]
    Open {
        #[mesh(1)]
        params: OpenRequest,
        #[mesh(2)]
        modify_state: ModifyState,
        #[mesh(3)]
        reserved_state: Option<ReservedState>,
    },
    #[mesh(4)]
    Closing {
        #[mesh(1)]
        params: OpenRequest,
        #[mesh(2)]
        reserved_state: Option<ReservedState>,
    },
    #[mesh(5)]
    ClosingReopen {
        #[mesh(1)]
        params: OpenRequest,
        #[mesh(2)]
        request: OpenRequest,
    },
    #[mesh(6)]
    Revoked,
}

impl ChannelState {
    fn save(value: &super::ChannelState) -> Option<Self> {
        Some(match value {
            super::ChannelState::Closed => ChannelState::Closed,
            super::ChannelState::Opening {
                request,
                reserved_state,
            } => ChannelState::Opening {
                request: OpenRequest::save(request),
                reserved_state: reserved_state.as_ref().map(ReservedState::save),
            },
            super::ChannelState::ClosingReopen { params, request } => ChannelState::ClosingReopen {
                params: OpenRequest::save(params),
                request: OpenRequest::save(request),
            },
            super::ChannelState::Open {
                params,
                modify_state,
                reserved_state,
            } => ChannelState::Open {
                params: OpenRequest::save(params),
                modify_state: ModifyState::save(modify_state),
                reserved_state: reserved_state.as_ref().map(ReservedState::save),
            },
            super::ChannelState::Closing {
                params,
                reserved_state,
            } => ChannelState::Closing {
                params: OpenRequest::save(params),
                reserved_state: reserved_state.as_ref().map(ReservedState::save),
            },

            super::ChannelState::Revoked => ChannelState::Revoked,
            super::ChannelState::Reoffered => ChannelState::Revoked,
            super::ChannelState::ClientReleased
            | super::ChannelState::ClosingClientRelease
            | super::ChannelState::OpeningClientRelease => return None,
        })
    }

    fn restore(&self) -> Result<super::ChannelState, RestoreError> {
        Ok(match self {
            ChannelState::Closed => super::ChannelState::Closed,
            ChannelState::Opening {
                request,
                reserved_state,
            } => super::ChannelState::Opening {
                request: request.restore(),
                reserved_state: reserved_state
                    .as_ref()
                    .map(ReservedState::restore)
                    .transpose()?,
            },
            ChannelState::ClosingReopen { params, request } => super::ChannelState::ClosingReopen {
                params: params.restore(),
                request: request.restore(),
            },
            ChannelState::Open {
                params,
                modify_state,
                reserved_state,
            } => super::ChannelState::Open {
                params: params.restore(),
                modify_state: modify_state.restore(),
                reserved_state: reserved_state
                    .as_ref()
                    .map(ReservedState::restore)
                    .transpose()?,
            },
            ChannelState::Closing {
                params,
                reserved_state,
            } => super::ChannelState::Closing {
                params: params.restore(),
                reserved_state: reserved_state
                    .as_ref()
                    .map(ReservedState::restore)
                    .transpose()?,
            },
            ChannelState::Revoked => {
                // Mark it reoffered for now. This may transition back to revoked in post_restore.
                super::ChannelState::Reoffered
            }
        })
    }
}

impl Display for ChannelState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match self {
            Self::Closed => "Closed",
            Self::Opening { .. } => "Opening",
            Self::Open { .. } => "Open",
            Self::Closing { .. } => "Closing",
            Self::ClosingReopen { .. } => "ClosingReopen",
            Self::Revoked => "Revoked",
        };
        write!(f, "{}", state)
    }
}

#[derive(Debug, Clone, Protobuf)]
#[mesh(package = "vmbus.server.channels")]
pub struct Gpadl {
    #[mesh(1)]
    pub id: u32,
    #[mesh(2)]
    pub channel_id: u32,
    #[mesh(3)]
    pub count: u16,
    #[mesh(4)]
    pub buf: Vec<u64>,
    #[mesh(5)]
    pub state: GpadlState,
}

impl Gpadl {
    fn save(gpadl_id: GpadlId, channel_id: ChannelId, gpadl: &super::Gpadl) -> Option<Self> {
        tracing::trace!(id = %gpadl_id.0, channel_id = %channel_id.0, "gpadl saved");
        Some(Gpadl {
            id: gpadl_id.0,
            channel_id: channel_id.0,
            count: gpadl.count,
            buf: gpadl.buf.clone(),
            state: match gpadl.state {
                super::GpadlState::InProgress => GpadlState::InProgress,
                super::GpadlState::Offered => GpadlState::Offered,
                super::GpadlState::Accepted => GpadlState::Accepted,
                super::GpadlState::TearingDown => GpadlState::TearingDown,
                super::GpadlState::OfferedTearingDown => return None,
            },
        })
    }

    fn restore(self, channel: &super::Channel) -> Result<super::Gpadl, RestoreError> {
        if self.state != GpadlState::InProgress {
            // Validate the range.
            gparange::validate_gpa_ranges(self.count.into(), &self.buf)?;
        }
        let (state, allow_revoked) = match self.state {
            GpadlState::InProgress => (super::GpadlState::InProgress, true),
            GpadlState::Offered => (super::GpadlState::Offered, false),
            GpadlState::Accepted => {
                // It is assumed the device already knows about this GPADL.
                (super::GpadlState::Accepted, true)
            }
            GpadlState::TearingDown => (super::GpadlState::TearingDown, false),
        };

        if !allow_revoked && channel.state.is_revoked() {
            return Err(RestoreError::GpadlForRevokedChannel(
                GpadlId(self.id),
                ChannelId(self.channel_id),
            ));
        }

        Ok(super::Gpadl {
            count: self.count,
            buf: self.buf,
            state,
        })
    }

    pub fn is_tearing_down(&self) -> bool {
        self.state == GpadlState::TearingDown
    }
}

#[derive(Debug, Clone, Protobuf, PartialEq, Eq)]
#[mesh(package = "vmbus.server.channels")]
pub enum GpadlState {
    #[mesh(1)]
    InProgress,
    #[mesh(2)]
    Offered,
    #[mesh(3)]
    Accepted,
    #[mesh(4)]
    TearingDown,
}

#[derive(Debug, Clone, Protobuf, PartialEq, Eq)]
#[mesh(package = "vmbus.server.channels")]
pub struct OutgoingMessage(pub Vec<u8>);

impl OutgoingMessage {
    fn save(value: &vmbus_core::OutgoingMessage) -> Self {
        Self(value.data().to_vec())
    }

    fn restore(self) -> Result<vmbus_core::OutgoingMessage, RestoreError> {
        vmbus_core::OutgoingMessage::from_message(&self.0)
            .map_err(|_| RestoreError::MessageTooLarge)
    }
}
