// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Saved state support for the vmbus client.

use crate::ConnectResult;
use crate::RestoreError;
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

        let core_state = self.core.save();
        let mut pending_messages = self
            .inner
            .messages
            .queued
            .iter()
            .map(|msg| PendingMessage {
                data: msg.data().to_vec(),
            })
            .collect::<Vec<_>>();
        if self.paused_via_message {
            pending_messages.insert(
                0,
                PendingMessage {
                    data: OutgoingMessage::new(&protocol::Resume).data().to_vec(),
                },
            );
        }
        pending_messages.extend(core_state.released_channel_ids.iter().map(|&channel_id| {
            PendingMessage {
                data: OutgoingMessage::new(&protocol::RelIdReleased { channel_id })
                    .data()
                    .to_vec(),
            }
        }));

        SavedState {
            client_state: match core_state.version {
                None => ClientState::Disconnected,
                Some(version) => ClientState::Connected {
                    version: version.version as u32,
                    feature_flags: version.feature_flags.into(),
                },
            },
            channels: core_state
                .channels
                .iter()
                .map(|channel| {
                    let state = match channel.phase {
                        vmbus_client_core::SavedChannelPhase::Offered => ChannelState::Offered,
                        vmbus_client_core::SavedChannelPhase::Opened => ChannelState::Opened,
                    };
                    let key = offer_key(&channel.offer);
                    tracing::info!(%key, %state, "channel saved");
                    Channel {
                        id: channel.offer.channel_id.0,
                        state,
                        offer: channel.offer.into(),
                    }
                })
                .collect(),
            gpadls: core_state
                .channels
                .iter()
                .flat_map(|channel| {
                    channel.gpadls.iter().map(|gpadl| Gpadl {
                        gpadl_id: gpadl.id.0,
                        channel_id: channel.offer.channel_id.0,
                        state: match gpadl.phase {
                            vmbus_client_core::SavedGpadlPhase::Created => GpadlState::Created,
                            vmbus_client_core::SavedGpadlPhase::TearingDown => {
                                GpadlState::TearingDown
                            }
                        },
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

        let version = match client_state {
            ClientState::Disconnected => None,
            ClientState::Connected {
                version,
                feature_flags,
            } => {
                let version = super::SUPPORTED_VERSIONS
                    .iter()
                    .find(|v| version == **v as u32)
                    .copied()
                    .ok_or(RestoreError::UnsupportedVersion(version))?;
                Some(VersionInfo {
                    version,
                    feature_flags: FeatureFlags::from(feature_flags),
                })
            }
        };

        let mut core_channels = channels
            .into_iter()
            .map(|channel| vmbus_client_core::SavedChannel {
                offer: channel.offer.into(),
                phase: match channel.state {
                    ChannelState::Offered => vmbus_client_core::SavedChannelPhase::Offered,
                    ChannelState::Opened => vmbus_client_core::SavedChannelPhase::Opened,
                },
                gpadls: Vec::new(),
            })
            .collect::<Vec<_>>();
        for gpadl in gpadls {
            let channel_id = ChannelId(gpadl.channel_id);
            let gpadl_id = GpadlId(gpadl.gpadl_id);
            let channel = core_channels
                .iter_mut()
                .find(|channel| channel.offer.channel_id == channel_id)
                .ok_or(RestoreError::GpadlForUnknownChannelId(channel_id.0))?;
            if channel.gpadls.iter().any(|gpadl| gpadl.id == gpadl_id) {
                return Err(RestoreError::DuplicateGpadlId(gpadl_id.0));
            }
            channel.gpadls.push(vmbus_client_core::SavedGpadl {
                id: gpadl_id,
                phase: match gpadl.state {
                    GpadlState::Created => vmbus_client_core::SavedGpadlPhase::Created,
                    GpadlState::TearingDown => vmbus_client_core::SavedGpadlPhase::TearingDown,
                },
            });
        }

        let core_state = vmbus_client_core::SavedState {
            version,
            channels: core_channels,
            released_channel_ids: Vec::new(),
        };
        self.core
            .restore(core_state.clone())
            .map_err(|error| match error {
                vmbus_client_core::RestoreError::UnsupportedVersion(version) => {
                    RestoreError::UnsupportedVersion(version)
                }
                vmbus_client_core::RestoreError::UnsupportedFeatureFlags(flags) => {
                    RestoreError::UnsupportedFeatureFlags(flags)
                }
                vmbus_client_core::RestoreError::DuplicateChannelId(channel_id) => {
                    RestoreError::DuplicateChannelId(channel_id)
                }
                vmbus_client_core::RestoreError::DuplicateGpadlId(gpadl_id) => {
                    RestoreError::DuplicateGpadlId(gpadl_id)
                }
            })?;
        self.runtime_channels.clear();
        self.offer_send = None;

        for message in pending_messages {
            self.inner.messages.queued.push_back(
                OutgoingMessage::from_message(&message.data)
                    .map_err(RestoreError::InvalidPendingMessage)?,
            );
        }

        let Some(version) = version else {
            return Ok(None);
        };
        let (offer_send, offer_recv) = mesh::channel();
        self.offer_send = Some(offer_send);
        let mut restored_channels = Vec::new();
        for channel in core_state.channels {
            let state = match channel.phase {
                vmbus_client_core::SavedChannelPhase::Offered => ChannelState::Offered,
                vmbus_client_core::SavedChannelPhase::Opened => ChannelState::Opened,
            };
            let offer_info = self
                .create_offer_info(channel.offer)
                .map_err(RestoreError::OfferFailed)?;
            let key = offer_key(&offer_info.offer);
            tracing::info!(%key, %state, "channel restored");
            restored_channels.push(offer_info);
        }

        Ok(Some(ConnectResult {
            version,
            offers: restored_channels,
            offer_recv,
        }))
    }

    pub fn handle_post_restore(&mut self) {
        assert!(!self.running);
        let mut sink = super::ActionBuffer::default();
        self.core.post_restore(&mut sink);
        let mut outcome = super::StepOutcome::default();
        for action in sink.actions {
            self.handle_action(action, &mut outcome);
        }
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
