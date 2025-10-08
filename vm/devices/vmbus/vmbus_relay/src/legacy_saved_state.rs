// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides saved state structures for compatibility with the 2411 release,
//! where the relay and `vmbus_client` were saved together as a single state
//! unit.

pub use relay::SavedState;

use crate::saved_state;

mod relay {
    use mesh::payload::Protobuf;
    use vmcore::save_restore::SavedStateRoot;

    /// The legacy, 2411 saved state for the relay.
    #[derive(Clone, Protobuf, SavedStateRoot)]
    #[mesh(package = "vmbus.relay")]
    pub struct SavedState {
        #[mesh(1)]
        pub(crate) use_interrupt_relay: bool,
        #[mesh(2)]
        pub(super) relay_state: RelayState,
        #[mesh(3)]
        pub(super) client_saved_state: vmbus_client::SavedState,
        #[mesh(4)]
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
    }

    #[derive(Copy, Clone, Eq, PartialEq, Protobuf)]
    #[mesh(package = "vmbus.relay")]
    pub(super) enum RelayState {
        #[mesh(1)]
        Disconnected,
        #[mesh(2)]
        Connected,
    }
}

impl SavedState {
    /// Constructs a legacy saved state from the current relay and client saved
    /// states.
    pub fn from_relay_and_client(
        relay: &saved_state::SavedState,
        client: &vmbus_client::SavedState,
    ) -> Self {
        Self {
            use_interrupt_relay: relay.use_interrupt_relay,
            relay_state: relay::RelayState::Connected,
            client_saved_state: client.clone(),
            channels: relay
                .channels
                .iter()
                .map(|channel| relay::Channel {
                    channel_id: channel.channel_id,
                    event_flag: channel.event_flag,
                    intercepted: channel.intercepted,
                    intercepted_save_state: channel.intercepted_save_state.clone(),
                })
                .collect(),
        }
    }

    /// Produces a current relay saved state from the legacy saved state.
    pub fn relay_saved_state(&mut self) -> saved_state::SavedState {
        self.client_saved_state
            .channels
            .sort_by_key(|channel| channel.offer.channel_id);

        saved_state::SavedState {
            use_interrupt_relay: self.use_interrupt_relay,
            channels: self
                .channels
                .iter()
                .map(|channel| saved_state::Channel {
                    channel_id: channel.channel_id,
                    event_flag: channel.event_flag,
                    intercepted: channel.intercepted,
                    intercepted_save_state: channel.intercepted_save_state.clone(),
                    is_open: self
                        .client_saved_state
                        .channels
                        .binary_search_by_key(&channel.channel_id, |channel| {
                            channel.offer.channel_id
                        })
                        .is_ok_and(|i| {
                            matches!(
                                self.client_saved_state.channels[i].state,
                                vmbus_client::saved_state::ChannelState::Opened
                            )
                        }),
                })
                .collect(),
        }
    }

    /// Produces a current client saved state from the legacy saved state.
    pub fn client_saved_state(&self) -> vmbus_client::SavedState {
        self.client_saved_state.clone()
    }
}
