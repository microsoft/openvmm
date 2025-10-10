// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Vmbus bus definitions.

use async_trait::async_trait;
use guestmem::GuestMemory;
use guid::Guid;
use inspect::Inspect;
use mesh::MeshPayload;
use mesh::payload::Protobuf;
use mesh::rpc::FailableRpc;
use mesh::rpc::Rpc;
use std::fmt::Display;
use std::time::Duration;
use vmbus_core::protocol;
use vmbus_core::protocol::GpadlId;
use vmbus_core::protocol::UserDefinedData;
use vmcore::interrupt::Interrupt;

/// Input for creating a channel offer.
#[derive(Debug)]
pub struct OfferInput {
    /// Parameters describing the offer.
    pub params: OfferParams,
    /// The event to signal when the guest needs attention.
    pub event: Interrupt,
    /// A mesh channel to send channel-related requests to.
    pub request_send: mesh::Sender<ChannelRequest>,
    /// A mesh channel to receive channel-related requests to.
    pub server_request_recv: mesh::Receiver<ChannelServerRequest>,
}

/// Resources for an offered channel.
#[derive(Debug, Default)]
pub struct OfferResources {
    /// Untrusted guest memory access.
    untrusted_memory: GuestMemory,
    /// Private guest memory access. This will be `None` unless running in a paravisor of a hardware
    /// isolated VM.
    private_memory: Option<GuestMemory>,
}

impl OfferResources {
    /// Creates a new `OfferResources`.
    pub fn new(untrusted_memory: GuestMemory, private_memory: Option<GuestMemory>) -> Self {
        OfferResources {
            untrusted_memory,
            private_memory,
        }
    }

    /// Returns the `GuestMemory` to use based on the whether the open request requests confidential
    /// memory.
    ///
    /// The open request reflects both whether the device indicated it supports confidential
    /// external memory when it was offered, and whether the currently connected vmbus client
    /// supports it. As such, you must not attempt to get the guest memory until a channel is
    /// opened, and you should not retain the guest memory after it is closed, as the client and
    /// its capabilities may change across opens.
    pub fn guest_memory(&self, open_request: &OpenRequest) -> &GuestMemory {
        self.get_memory(open_request.use_confidential_external_memory)
    }

    pub(crate) fn ring_memory(&self, open_request: &OpenRequest) -> &GuestMemory {
        self.get_memory(open_request.use_confidential_ring)
    }

    fn get_memory(&self, private: bool) -> &GuestMemory {
        if private {
            self.private_memory
                .as_ref()
                .expect("private memory should be present if confidential memory is requested")
        } else {
            &self.untrusted_memory
        }
    }
}

/// A request from the VMBus control plane.
#[derive(Debug, MeshPayload)]
pub enum ChannelRequest {
    /// Open the channel.
    Open(Rpc<OpenRequest, bool>),
    /// Close the channel.
    ///
    /// Although there is no response from the host, this is still modeled as an
    /// RPC so that the caller can know that the vmbus client's state has been
    /// updated.
    Close(Rpc<(), ()>),
    /// Create a new GPADL.
    Gpadl(Rpc<GpadlRequest, bool>),
    /// Tear down an existing GPADL.
    TeardownGpadl(Rpc<GpadlId, ()>),
    /// Modify the channel's target VP.
    Modify(Rpc<ModifyRequest, i32>),
}

/// GPADL information from the guest.
#[derive(Debug, MeshPayload)]
pub struct GpadlRequest {
    /// The GPADL ID.
    pub id: GpadlId,
    /// The number of ranges in the GPADL.
    pub count: u16,
    /// The GPA range buffer.
    pub buf: Vec<u64>,
}

/// Modify channel request.
#[derive(Debug, MeshPayload)]
pub enum ModifyRequest {
    /// Change the target VP to `target_vp`.
    TargetVp {
        /// The new target VP.
        target_vp: u32,
    },
}

/// A request to the VMBus control plane.
#[derive(mesh::MeshPayload)]
pub enum ChannelServerRequest {
    /// A request to restore the channel.
    ///
    /// The input parameter indicates if the channel was saved open.
    Restore(FailableRpc<bool, RestoreResult>),
    /// A request to revoke the channel.
    ///
    /// A channel can also be revoked by dropping it. This request is only necessary if you need to
    /// wait for the revoke operation to complete.
    Revoke(Rpc<(), ()>),
}

/// The result of a [`ChannelServerRequest::Restore`] operation.
#[derive(Debug, MeshPayload)]
pub struct RestoreResult {
    /// The open request, if the channel was opened restored.
    pub open_request: Option<OpenRequest>,
    /// The active GPADLs.
    pub gpadls: Vec<RestoredGpadl>,
}

/// A restored GPADL.
#[derive(Debug, MeshPayload)]
pub struct RestoredGpadl {
    /// The GPADL request.
    pub request: GpadlRequest,
    /// Whether the GPADL was saved in the accepted state.
    ///
    /// If true, failure to restore this is fatal to the restore operation. If
    /// false, the device will later get another GPADL offer for this same
    /// GPADL.
    ///
    /// This is needed because the device may have saved itself with a
    /// dependency on this GPADL even if the response did not make it into the
    /// vmbus server saved state.
    pub accepted: bool,
}

/// Trait implemented by VMBus servers.
#[async_trait]
pub trait ParentBus: Send + Sync {
    /// Offers a new channel.
    async fn add_child(&self, request: OfferInput) -> anyhow::Result<OfferResources>;

    /// Clones the bus.
    ///
    /// TODO: This is needed for now to support transparent subchannel offers.
    /// Remove this once subchannels can be pre-created at primary channel offer
    /// time.
    fn clone_bus(&self) -> Box<dyn ParentBus>;

    /// Returns whether [`OfferInput::event`] needs to be backed by an OS event.
    ///
    /// TODO: Remove this and just return the appropriate notify type directly
    /// once subchannel creation and enable are separated.
    fn use_event(&self) -> bool {
        true
    }
}

/// Channel open-specific data.
#[derive(Debug, Copy, Clone, mesh::MeshPayload)]
pub struct OpenData {
    /// The target VP for interrupts to the guest.
    pub target_vp: u32,
    /// The page offset into the ring GPADL of the host-to-guest ring buffer.
    pub ring_offset: u32,
    /// The ring buffer's GPADL ID.
    pub ring_gpadl_id: GpadlId,
    /// The event flag used to notify the guest.
    pub event_flag: u16,
    /// An connection ID used when the guest notifies the host.
    pub connection_id: u32,
    /// User data provided by the opener.
    pub user_data: UserDefinedData,
}

/// Information provided to devices when a channel is opened.
#[derive(Debug, Clone, mesh::MeshPayload)]
pub struct OpenRequest {
    /// Channel open-specific data.
    pub open_data: OpenData,
    /// The interrupt used to signal the guest.
    pub interrupt: Interrupt,
    /// Indicates if the currently connected vmbus client, as well as the channel the request is
    /// for, supports the use of confidential ring buffers.
    pub use_confidential_ring: bool,
    /// Indicates if the currently connected vmbus client, as well as the channel the request is
    /// for, supports the use of confidential external memory.
    pub use_confidential_external_memory: bool,
}

impl OpenRequest {
    /// Creates a new `OpenRequest`.
    pub fn new(
        open_data: OpenData,
        interrupt: Interrupt,
        feature_flags: protocol::FeatureFlags,
        offer_flags: protocol::OfferFlags,
    ) -> Self {
        Self {
            open_data,
            interrupt,
            use_confidential_ring: feature_flags.confidential_channels()
                && offer_flags.confidential_ring_buffer(),
            use_confidential_external_memory: feature_flags.confidential_channels()
                && offer_flags.confidential_external_memory(),
        }
    }
}

#[derive(Debug, Default, Copy, Clone, Hash, Eq, PartialEq, Ord, PartialOrd, Protobuf, Inspect)]
/// The identifying IDs for a channel offer.
#[mesh(package = "vmbus")]
pub struct OfferKey {
    /// The interface ID describing the type of channel.
    #[mesh(1)]
    pub interface_id: Guid,
    /// The unique instance ID for the channel.
    #[mesh(2)]
    pub instance_id: Guid,
    /// The subchannel index. Index 0 indicates a primary (normal channel).
    #[mesh(3)]
    pub subchannel_index: u16,
}

impl Display for OfferKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{{{}}}-{{{}}}-{}",
            self.interface_id, self.instance_id, self.subchannel_index
        )
    }
}

impl From<&protocol::OfferChannel> for OfferKey {
    fn from(offer: &protocol::OfferChannel) -> Self {
        Self {
            interface_id: offer.interface_id,
            instance_id: offer.instance_id,
            subchannel_index: offer.subchannel_index,
        }
    }
}

/// Channel offer parameters.
#[derive(Debug, Clone, Default, mesh::MeshPayload)]
pub struct OfferParams {
    /// An informational string describing the channel type.
    pub interface_name: String,
    /// The unique instance ID for the channel.
    pub instance_id: Guid,
    /// The interface ID describing the type of channel.
    pub interface_id: Guid,
    /// The amount of MMIO space needed by the channel, in megabytes.
    pub mmio_megabytes: u16,
    /// The amount of optional MMIO space used by the channel, in megabytes.
    pub mmio_megabytes_optional: u16,
    /// The channel's type.
    pub channel_type: ChannelType,
    /// The subchannel index. Index 0 indicates a primary (normal channel).
    pub subchannel_index: u16,
    /// Indicates whether the channel's interrupts should use monitor pages,
    /// and the interrupt latency if it's enabled.
    pub mnf_interrupt_latency: Option<Duration>,
    /// The order in which channels with the same interface will be offered to
    /// the guest (optional).
    pub offer_order: Option<u64>,
    /// Indicates whether the channel supports using encrypted memory for any
    /// external GPADLs and GPA direct ranges. This is only used when hardware
    /// isolation is in use.
    pub allow_confidential_external_memory: bool,
}

impl OfferParams {
    /// Gets the offer key for this offer.
    pub fn key(&self) -> OfferKey {
        OfferKey {
            interface_id: self.interface_id,
            instance_id: self.instance_id,
            subchannel_index: self.subchannel_index,
        }
    }
}

/// The channel type.
#[derive(Debug, Copy, Clone, MeshPayload, Inspect)]
#[inspect(external_tag)]
pub enum ChannelType {
    /// A channel representing a device.
    Device {
        /// If true, the ring buffer packets should contain pipe headers.
        pipe_packets: bool,
    },
    /// A channel representing an interface for the guest to open.
    Interface {
        /// Interface-specific user-defined data to put in the channel offer.
        user_defined: UserDefinedData,
    },
    /// A channel representing a pipe.
    Pipe {
        /// If true, the pipe uses message mode. Otherwise, it uses byte mode.
        message_mode: bool,
    },
    /// A channel representing a Hyper-V socket.
    HvSocket {
        /// If true, this is a connect to the guest. Otherwise, this is a
        /// connect from the guest.
        is_connect: bool,
        /// If true, the connection is for a container in the guest.
        is_for_container: bool,
        /// The silo ID to connect to. Use `Guid::ZERO` to not specify a silo ID.
        silo_id: Guid,
    },
}

impl Default for ChannelType {
    fn default() -> Self {
        Self::Device {
            pipe_packets: false,
        }
    }
}
