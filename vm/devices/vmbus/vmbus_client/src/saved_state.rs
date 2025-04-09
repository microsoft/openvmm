// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::ConnectResult;
use crate::OfferInfo;
use crate::RestoreError;
use crate::SUPPORTED_FEATURE_FLAGS;
use guid::Guid;
use mesh::payload::Protobuf;
use vmbus_channel::bus::OfferKey;
use vmbus_core::OutgoingMessage;
use vmbus_core::VersionInfo;
use vmbus_core::protocol;
use vmbus_core::protocol::ChannelId;
use vmbus_core::protocol::FeatureFlags;
use vmbus_core::protocol::GpadlId;

impl super::ClientTask {
    pub fn handle_save(&mut self) -> SavedState {
        assert!(!self.running);

        let mut pending_messages = self
            .inner
            .messages
            .queued
            .iter()
            .map(|msg| PendingMessage {
                data: msg.data().to_vec(),
            })
            .collect::<Vec<_>>();

        // It's the responsibility of the caller to ensure the client is in a state where it's
        // possible to save.
        SavedState {
            client_state: match self.state {
                super::ClientState::Disconnected => ClientState::Disconnected,
                super::ClientState::Connecting { .. } => {
                    unreachable!("Cannot save in Connecting state.")
                }
                super::ClientState::Connected { version, .. } => ClientState::Connected {
                    version: version.version as u32,
                    feature_flags: version.feature_flags.into(),
                },
                super::ClientState::RequestingOffers { .. } => {
                    unreachable!("Cannot save in RequestingOffers state.")
                }
                super::ClientState::Disconnecting { .. } => {
                    unreachable!("Cannot save in Disconnecting state.")
                }
            },
            channels: self
                .inner
                .channels
                .0
                .iter()
                .filter_map(|(&id, v)| {
                    let Some(v) = v else {
                        // The channel has been revoked, but the user is not
                        // done with it. The channel won't be available for use
                        // when we restore, so don't save it, but do save a
                        // pending message to the server to release the channel
                        // ID.
                        pending_messages.push(PendingMessage {
                            data: OutgoingMessage::new(&protocol::RelIdReleased { channel_id: id })
                                .data()
                                .to_vec(),
                        });
                        return None;
                    };
                    assert!(
                        v.modify_response_send.is_none(),
                        "Cannot save a channel that is being modified."
                    );
                    let key = offer_key(&v.offer);
                    tracing::info!(%key, %v.state, "channel saved");
                    Some(Channel {
                        id: id.0,
                        state: ChannelState::save(&v.state),
                        offer: v.offer.into(),
                    })
                })
                .collect(),
            gpadls: self
                .inner
                .channels
                .0
                .iter()
                .flat_map(|(channel_id, channel)| {
                    channel.iter().flat_map(|c| {
                        c.gpadls.iter().map(|(gpadl_id, gpadl_state)| Gpadl {
                            gpadl_id: gpadl_id.0,
                            channel_id: channel_id.0,
                            state: GpadlState::save(gpadl_state),
                        })
                    })
                })
                .collect(),
            pending_messages,
        }
    }

    pub fn handle_restore(
        &mut self,
        saved_state: SavedState,
    ) -> Result<Option<ConnectResult>, RestoreError> {
        assert!(!self.running);

        let SavedState {
            client_state,
            channels,
            gpadls,
            pending_messages,
        } = saved_state;

        let (version, feature_flags) = match client_state {
            ClientState::Disconnected => return Ok(None),
            ClientState::Connected {
                version,
                feature_flags,
            } => (version, feature_flags),
        };

        let version = super::SUPPORTED_VERSIONS
            .iter()
            .find(|v| version == **v as u32)
            .copied()
            .ok_or(RestoreError::UnsupportedVersion(version))?;

        let feature_flags = FeatureFlags::from(feature_flags);
        if !SUPPORTED_FEATURE_FLAGS.contains(feature_flags) {
            return Err(RestoreError::UnsupportedFeatureFlags(feature_flags.into()));
        }

        let version = VersionInfo {
            version,
            feature_flags,
        };

        let (offer_send, offer_recv) = mesh::channel();
        self.state = super::ClientState::Connected {
            version,
            offer_send,
        };

        let mut restored_channels = Vec::new();
        for saved_channel in channels {
            let offer_info = self.restore_channel(saved_channel)?;
            let key = offer_key(&offer_info.offer);
            tracing::info!(%key, state = %saved_channel.state, "channel restored");
            restored_channels.push(offer_info);
        }

        for gpadl in gpadls {
            let channel_id = ChannelId(gpadl.channel_id);
            let gpadl_id = GpadlId(gpadl.gpadl_id);
            let gpadl_state = gpadl.state.restore();
            let tearing_down = matches!(gpadl_state, super::GpadlState::TearingDown { .. });

            let channel = self
                .inner
                .channels
                .0
                .get_mut(&channel_id)
                .and_then(|v| v.as_mut())
                .ok_or(RestoreError::GpadlForUnknownChannelId(channel_id.0))?;

            if channel.gpadls.insert(gpadl_id, gpadl_state).is_some() {
                return Err(RestoreError::DuplicateGpadlId(gpadl_id.0));
            }

            if tearing_down
                && self
                    .inner
                    .teardown_gpadls
                    .insert(gpadl_id, channel_id)
                    .is_some()
            {
                unreachable!("gpadl ID validated above");
            }
        }

        for message in pending_messages {
            self.inner.messages.queued.push_back(
                OutgoingMessage::from_message(&message.data)
                    .map_err(RestoreError::InvalidPendingMessage)?,
            );
        }

        Ok(Some(ConnectResult {
            version,
            offers: restored_channels,
            offer_recv,
        }))
    }

    pub fn handle_post_restore(&mut self) {
        assert!(!self.running);

        // Close restored channels that have not been claimed.
        for (&channel_id, channel) in &mut self.inner.channels.0 {
            let Some(channel) = channel else { continue };
            if let super::ChannelState::Restored = channel.state {
                tracing::info!(
                    channel_id = channel_id.0,
                    "closing unclaimed restored channel"
                );
                self.inner
                    .messages
                    .send(&protocol::CloseChannel { channel_id });
                channel.state = super::ChannelState::Offered;

                for (&gpadl_id, gpadl_state) in &mut channel.gpadls {
                    // FUTURE: wait for GPADL teardown so that everything is in a clean
                    // state after this.
                    match gpadl_state {
                        crate::GpadlState::Offered(_) => unreachable!(),
                        crate::GpadlState::Created => {
                            self.inner.teardown_gpadls.insert(gpadl_id, channel_id);
                            self.inner.messages.send(&protocol::GpadlTeardown {
                                channel_id,
                                gpadl_id,
                            });
                            *gpadl_state = crate::GpadlState::TearingDown { rpcs: Vec::new() };
                        }
                        crate::GpadlState::TearingDown { .. } => {}
                    }
                }
            }
        }
    }

    fn restore_channel(&mut self, channel: Channel) -> Result<OfferInfo, RestoreError> {
        self.create_channel_core(channel.offer.into(), channel.state.restore())
            .map_err(RestoreError::OfferFailed)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "vmbus.client")]
pub struct SavedState {
    #[mesh(1)]
    pub client_state: ClientState,
    #[mesh(2)]
    pub channels: Vec<Channel>,
    #[mesh(3)]
    pub gpadls: Vec<Gpadl>,
    /// Added in Feb 2025, but not yet used in practice (we flush pending
    /// messages during stop) since we need to support restoring on older
    /// versions.
    #[mesh(4)]
    pub pending_messages: Vec<PendingMessage>,
}

#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "vmbus.client")]
pub struct PendingMessage {
    #[mesh(1)]
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "vmbus.client")]
pub enum ClientState {
    #[mesh(1)]
    Disconnected,
    #[mesh(2)]
    Connected {
        #[mesh(1)]
        version: u32,
        #[mesh(2)]
        feature_flags: u32,
    },
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Protobuf)]
#[mesh(package = "vmbus.client")]
pub struct Channel {
    #[mesh(1)]
    pub id: u32,
    #[mesh(2)]
    pub state: ChannelState,
    #[mesh(3)]
    pub offer: Offer,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Protobuf)]
#[mesh(package = "vmbus.client")]
pub enum ChannelState {
    #[mesh(1)]
    Offered,
    #[mesh(2)]
    Opened,
}

impl ChannelState {
    fn save(state: &super::ChannelState) -> Self {
        match state {
            super::ChannelState::Offered => Self::Offered,
            super::ChannelState::Opening { .. } => {
                unreachable!("Cannot save channel in opening state.")
            }
            super::ChannelState::Restored | super::ChannelState::Opened { .. } => Self::Opened,
        }
    }

    fn restore(self) -> super::ChannelState {
        match self {
            ChannelState::Offered => super::ChannelState::Offered,
            ChannelState::Opened => super::ChannelState::Restored,
        }
    }
}

impl std::fmt::Display for ChannelState {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChannelState::Offered => write!(fmt, "Offered"),
            ChannelState::Opened => write!(fmt, "Opened"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "vmbus.client")]
pub enum GpadlState {
    #[mesh(1)]
    Created,
    #[mesh(2)]
    TearingDown,
}

impl GpadlState {
    fn save(value: &super::GpadlState) -> Self {
        match value {
            super::GpadlState::Offered(..) => unreachable!("Cannot save gpadl in offered state."),
            super::GpadlState::Created => Self::Created,
            super::GpadlState::TearingDown { .. } => Self::TearingDown,
        }
    }

    fn restore(self) -> super::GpadlState {
        match self {
            GpadlState::Created => super::GpadlState::Created,
            GpadlState::TearingDown => super::GpadlState::TearingDown { rpcs: Vec::new() },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "vmbus.client")]
pub struct Gpadl {
    #[mesh(1)]
    pub gpadl_id: u32,
    #[mesh(2)]
    pub channel_id: u32,
    #[mesh(3)]
    pub state: GpadlState,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "vmbus.client")]
pub struct Offer {
    #[mesh(1)]
    pub interface_id: Guid,
    #[mesh(2)]
    pub instance_id: Guid,
    #[mesh(3)]
    pub flags: u16,
    #[mesh(4)]
    pub mmio_megabytes: u16,
    #[mesh(5)]
    pub user_defined: [u8; 120],
    #[mesh(6)]
    pub subchannel_index: u16,
    #[mesh(7)]
    pub mmio_megabytes_optional: u16,
    #[mesh(8)]
    pub channel_id: u32,
    #[mesh(9)]
    pub monitor_id: u8,
    #[mesh(10)]
    pub monitor_allocated: u8,
    #[mesh(11)]
    pub is_dedicated: u16,
    #[mesh(12)]
    pub connection_id: u32,
}

impl From<protocol::OfferChannel> for Offer {
    fn from(offer: protocol::OfferChannel) -> Self {
        Self {
            interface_id: offer.interface_id,
            instance_id: offer.instance_id,
            flags: offer.flags.into(),
            mmio_megabytes: offer.mmio_megabytes,
            user_defined: offer.user_defined.into(),
            subchannel_index: offer.subchannel_index,
            mmio_megabytes_optional: offer.mmio_megabytes_optional,
            channel_id: offer.channel_id.0,
            monitor_id: offer.monitor_id,
            monitor_allocated: offer.monitor_allocated,
            is_dedicated: offer.is_dedicated,
            connection_id: offer.connection_id,
        }
    }
}

impl From<Offer> for protocol::OfferChannel {
    fn from(offer: Offer) -> Self {
        Self {
            interface_id: offer.interface_id,
            instance_id: offer.instance_id,
            flags: offer.flags.into(),
            rsvd: [0; 4],
            mmio_megabytes: offer.mmio_megabytes,
            user_defined: offer.user_defined.into(),
            subchannel_index: offer.subchannel_index,
            mmio_megabytes_optional: offer.mmio_megabytes_optional,
            channel_id: ChannelId(offer.channel_id),
            monitor_id: offer.monitor_id,
            monitor_allocated: offer.monitor_allocated,
            is_dedicated: offer.is_dedicated,
            connection_id: offer.connection_id,
        }
    }
}

fn offer_key(offer: &protocol::OfferChannel) -> OfferKey {
    OfferKey {
        interface_id: offer.interface_id,
        instance_id: offer.instance_id,
        subchannel_index: offer.subchannel_index,
    }
}
