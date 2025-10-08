// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::ChannelId;
use crate::ChannelInfo;
use crate::InterceptChannelRequest;
use crate::InterruptRelay;
use crate::RelayChannelRequest;
use crate::RelayChannelTask;
use crate::RelayTask;
use anyhow::Context as _;
use anyhow::Result;
use mesh::payload::Protobuf;
use mesh::rpc::RpcSend;
use pal_event::Event;
use std::sync::atomic::Ordering;
use vmbus_channel::bus::ChannelServerRequest;
use vmbus_client as client;
use vmcore::notify::Notify;
use vmcore::save_restore::SavedStateRoot;

impl RelayTask {
    pub async fn handle_save(&self) -> SavedState {
        assert!(!self.running);

        let channels = futures::future::join_all(
            self.channels
                .iter()
                .map(|(id, channel)| self.save_channel_state(*id, channel)),
        )
        .await
        .drain(..)
        .flatten()
        .collect();

        SavedState {
            use_interrupt_relay: self.use_interrupt_relay.load(Ordering::SeqCst),
            channels,
        }
    }

    pub async fn handle_restore(&mut self, state: SavedState) -> Result<()> {
        let SavedState {
            use_interrupt_relay,
            channels,
        } = state;

        self.use_interrupt_relay
            .store(use_interrupt_relay, Ordering::SeqCst);

        for saved_channel in channels {
            let Some(channel) = self.channels.get_mut(&ChannelId(saved_channel.channel_id)) else {
                tracing::info!(
                    channel_id = saved_channel.channel_id,
                    "channel not found during restore, probably revoked"
                );
                continue;
            };
            match channel {
                ChannelInfo::Relay(info) => {
                    info.relay_request_send
                        .call_failable(RelayChannelRequest::Restore, saved_channel)
                        .await?;
                }
                ChannelInfo::Intercept(id) => {
                    if saved_channel.is_open {
                        anyhow::bail!("cannot restore intercepted channel {id}");
                    }
                }
            }
        }

        Ok(())
    }

    async fn save_channel_state(
        &self,
        channel_id: ChannelId,
        channel: &ChannelInfo,
    ) -> Option<Channel> {
        match channel {
            ChannelInfo::Relay(relay) => {
                match relay
                    .relay_request_send
                    .call(RelayChannelRequest::Save, ())
                    .await
                {
                    Ok(result) => Some(result),
                    Err(err) => {
                        tracing::error!(
                            err = &err as &dyn std::error::Error,
                            "Failed to save relay channel state"
                        );
                        None
                    }
                }
            }
            ChannelInfo::Intercept(id) => {
                let intercepted_save_state = if let Some(intercepted_channel) =
                    self.intercept_channels.get(id)
                {
                    let result = intercepted_channel
                        .call(InterceptChannelRequest::Save, ())
                        .await;
                    match result {
                        Ok(save_state) => mesh_protobuf::encode(save_state),
                        Err(err) => {
                            tracing::error!(err = &err as &dyn std::error::Error, %id, "Failed to call device to save state");
                            Vec::new()
                        }
                    }
                } else {
                    tracing::error!(%id, "Intercepted device missing during save operation");
                    Vec::new()
                };
                Some(Channel {
                    channel_id: channel_id.0,
                    event_flag: None,
                    intercepted: true,
                    intercepted_save_state,
                    is_open: false,
                })
            }
        }
    }
}

impl RelayChannelTask {
    /// Handle creating channel save state.
    pub(crate) fn handle_save(&self) -> Channel {
        Channel {
            channel_id: self.channel.channel_id.0,
            event_flag: self
                .channel
                .interrupt_relay
                .as_ref()
                .map(|interrupt| interrupt.event_flag),
            intercepted: false,
            intercepted_save_state: Vec::new(),
            is_open: self.channel.is_open,
        }
    }

    pub(crate) async fn handle_restore(&mut self, state: Channel) -> Result<()> {
        let Channel {
            channel_id: _,
            event_flag,
            intercepted,
            intercepted_save_state: _,
            is_open,
        } = state;

        if intercepted {
            anyhow::bail!("cannot restore an intercepted channel");
        }

        let result = self
            .channel
            .server_request_send
            .call(ChannelServerRequest::Restore, is_open)
            .await
            .context("Failed to send restore request")?
            .map_err(|err| {
                anyhow::Error::from(err).context("failed to restore vmbus relay channel")
            })?;

        if let Some(request) = result.open_request {
            let use_interrupt_relay = self.channel.use_interrupt_relay.load(Ordering::SeqCst);
            if use_interrupt_relay && event_flag.is_none() {
                anyhow::bail!("using an interrupt relay but no event flag was provided");
            }
            let (incoming_event, notify) = if use_interrupt_relay {
                let event = Event::new();
                let notify = Notify::from_event(event.clone())
                    .pollable(self.driver.as_ref())
                    .context("failed to create polled notify")?;
                Some((event, notify))
            } else {
                None
            }
            .unzip();

            self.channel
                .request_send
                .call_failable(
                    client::ChannelRequest::Restore,
                    client::RestoreRequest {
                        connection_id: request.open_data.connection_id,
                        redirected_event_flag: event_flag,
                        incoming_event,
                    },
                )
                .await
                .context("client failed to restore channel")?;

            if let Some(notify) = notify {
                self.channel.interrupt_relay = Some(InterruptRelay {
                    event_flag: event_flag.unwrap(),
                    notify,
                    interrupt: request.interrupt,
                });
            }
        }

        Ok(())
    }
}

#[derive(Clone, Protobuf, SavedStateRoot)]
#[mesh(package = "vmbus.relay")]
pub struct SavedState {
    #[mesh(1)]
    pub(crate) use_interrupt_relay: bool,
    // Fields 2, 3, and 4 are used by the legacy saved state but are ignored here.
    #[mesh(5)]
    pub(crate) channels: Vec<Channel>,
}

#[derive(Clone, Protobuf)]
#[mesh(package = "vmbus.relay")]
pub(crate) struct Channel {
    #[mesh(1)]
    pub(crate) channel_id: u32,
    #[mesh(2)]
    pub(crate) event_flag: Option<u16>,
    #[mesh(3)]
    pub(crate) intercepted: bool,
    #[mesh(4)]
    pub(crate) intercepted_save_state: Vec<u8>,
    #[mesh(5)]
    pub(crate) is_open: bool,
}
