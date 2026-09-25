// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `no_std` state machine at the core of the VMBus client.
//!
//! This crate factors the protocol-level state machine out of
//! [`vmbus_client`], so that it can be reused by both:
//!
//! * the mesh/async wrapper in [`vmbus_client`] (used by OpenHCL), and
//! * a polling wrapper in `opentmk::vmbus_guest` (used inside UEFI).
//!
//! # Design boundary
//!
//! * **Protocol semantics live in this crate.** Version negotiation,
//!   channel offer/open/close/release lifecycle, gpadl
//!   establish/teardown, hvsock connect tracking, unload — all here.
//!
//! * **Runtime concerns stay in the wrappers.** This crate does not
//!   touch `mesh`, `pal_event::Event`, `pal_async`, futures, tasks, or
//!   wakers.
//!
//! * `vmbus_client`'s public API does not change. The mesh wrapper
//!   translates its inputs into [`Event`]s, feeds them to
//!   `ClientCore::step`, and dispatches the resulting [`Action`]s back
//!   through its mesh/pal_async channels.
//!
//! # Boundary: [`Event`] in, [`Action`] out
//!
//! `ClientCore::step` is synchronous, single-threaded, and
//! deterministic. One input event may produce zero or more output
//! actions, streamed through an [`ActionSink`] to avoid allocating an
//! output `Vec` per step.
//!
//! [`vmbus_client`]: https://microsoft.github.io/openvmm/api/vmbus_client

#![no_std]
#![expect(missing_docs)]

use guid::Guid;
use vmbus_core::VersionInfo;
use vmbus_core::protocol::FeatureFlags;
use vmbus_core::protocol::Version;

// -- Configuration and identifiers -----------------------------------------

/// Opaque identifier the wrapper attaches to caller-initiated requests
/// so completions can be routed back. Monotonic, allocated by the
/// wrapper.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct RequestId(pub u64);

/// Immutable configuration for a [`ClientCore`] instance.
#[derive(Clone, Debug)]
pub struct Config {
    /// The synthetic interrupt used for VMBus channel-manager messages.
    /// Standard value is [`vmbus_core::VMBUS_SINT`] (2).
    pub sint: u8,
    /// The VTL the client runs in. Standard value is 0 (VTL0 client)
    /// or 2 (VTL2 openhcl-side client).
    pub vtl: u8,
    /// The protocol versions this client will offer, most preferred
    /// first.
    pub supported_versions: &'static [Version],
    /// The feature flags this client will advertise on
    /// `InitiateContact2`.
    pub supported_feature_flags: FeatureFlags,
}

/// Client identifier the host echoes back in `VersionResponse2`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ClientId(pub Guid);

// -- Top-level state machine -----------------------------------------------

/// The observable protocol phase of the [`ClientCore`].
///
/// This mirrors the guest's original `ClientState` from
/// `vmbus_client::lib`, minus the [`Rpc`] and [`mesh::Sender`] handles
/// that used to live on the enum variants. Wrappers track pending
/// completions externally through the [`RequestId`] they supplied on
/// the original [`Event`].
///
/// [`Rpc`]: https://microsoft.github.io/openvmm/api/mesh_rpc/struct.Rpc.html
/// [`mesh::Sender`]: https://microsoft.github.io/openvmm/api/mesh/struct.Sender.html
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum ClientPhase {
    /// The client has yet to connect to the host.
    #[default]
    Disconnected,
    /// The client has posted `InitiateContact` and is waiting for
    /// `VersionResponse`. `request_id` refers back to the caller-side
    /// `Connect` [`Event`] so the wrapper can complete the pending
    /// call when the negotiation finishes.
    Connecting {
        version: Version,
        request_id: RequestId,
    },
    /// The client has completed version negotiation and is ready to
    /// enumerate offers or perform per-channel operations.
    Connected { version: VersionInfo },
    /// The client has posted `RequestOffers` and is accumulating
    /// `OfferChannel`s until it observes `AllOffersDelivered`.
    /// `offer_count` is the number of offers the core has already
    /// forwarded to the wrapper via [`Action::OfferReceived`].
    RequestingOffers {
        version: VersionInfo,
        request_id: RequestId,
        offer_count: usize,
    },
    /// The client has posted `Unload` and is waiting for
    /// `UnloadComplete`.
    Disconnecting {
        version: VersionInfo,
        request_id: RequestId,
    },
}

impl ClientPhase {
    /// Returns the negotiated [`VersionInfo`] if the client has
    /// completed version negotiation.
    pub fn version(&self) -> Option<VersionInfo> {
        match self {
            Self::Connected { version, .. }
            | Self::RequestingOffers { version, .. }
            | Self::Disconnecting { version, .. } => Some(*version),
            Self::Disconnected | Self::Connecting { .. } => None,
        }
    }
}

// -- Event and Action skeletons --------------------------------------------
//
// The full [`Event`] and [`Action`] enums, along with `ClientCore` and
// the [`ActionSink`] trait, arrive in follow-up commits. This first
// commit lays down the crate structure and pins the load-bearing
// design decisions (RequestId, ClientPhase without Rpc fields, no
// mesh/pal_async/futures dependency) so the migration can proceed in
// well-scoped steps against a stable set of top-level types.

/// Placeholder for the input event type. Full enum arrives in a
/// follow-up commit.
#[non_exhaustive]
#[derive(Debug)]
pub enum Event {}

/// Placeholder for the output action type. Full enum arrives in a
/// follow-up commit.
#[non_exhaustive]
#[derive(Debug)]
pub enum Action {
    /// An offer arrived from the host after connect. Placeholder
    /// variant — the full offer payload shape lands in a follow-up
    /// commit.
    OfferReceived,
}

/// Placeholder for the top-level state machine. Full type + methods
/// arrive in a follow-up commit.
///
/// The real `ClientCore` will hold [`Config`], [`ClientPhase`], and
/// the per-channel/gpadl/hvsock/outgoing-message maps, and expose
/// `step(event, sink)` for the wrapper to drive.
#[non_exhaustive]
#[derive(Debug)]
pub struct ClientCore;

/// The sink through which [`Action`]s are emitted during a call to
/// `ClientCore::step`. Full implementation arrives in a follow-up
/// commit.
pub trait ActionSink {
    fn emit(&mut self, action: Action);
}
