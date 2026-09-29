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

extern crate alloc;

use guid::Guid;
use vmbus_core::VersionInfo;
use vmbus_core::protocol::FeatureFlags;
use vmbus_core::protocol::Version;

// -- Configuration and identifiers -----------------------------------------

/// Opaque identifier the wrapper attaches to caller-initiated requests
/// so completions can be routed back. Monotonic, allocated by the
/// wrapper.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
    /// call when the negotiation finishes. `params` are the original
    /// parameters supplied by the caller — they are reused verbatim
    /// on each rung of the ladder while the host rejects versions.
    Connecting {
        version: Version,
        request_id: RequestId,
        params: ConnectParams,
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

// -- Per-channel and per-gpadl state ---------------------------------------

/// Per-channel protocol state.
///
/// Mirrors `vmbus_client`'s internal `ChannelState` minus the runtime
/// handles that used to sit on the variants (`FailableRpc` for
/// `Opening`, `pal_event::Event` for the redirected-event mapping).
/// The wrapper tracks pending open completions through the `request_id`
/// carried on `Opening`, and owns the mapping from `redirected_event_flag`
/// to a live `pal_event::Event`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelPhase {
    /// The channel has been offered to the client by the host.
    Offered,
    /// The client has posted `OpenChannel[2]` and is waiting for
    /// `OpenResult`.
    Opening {
        /// Corresponds to the caller-side `OpenChannel` [`Event`] so
        /// the wrapper can complete the pending open when
        /// `OpenResult` arrives.
        request_id: RequestId,
        /// The wrapper-allocated redirected-event flag, if any. The
        /// wrapper owns the mapping from this flag to the concrete
        /// `pal_event::Event` on OpenHCL, or an in-guest interrupt on
        /// opentmk.
        redirected_event_flag: Option<u16>,
    },
    /// The channel has been restored from saved state but not yet
    /// claimed by a live open.
    Restored,
    /// The channel has been successfully opened.
    Opened {
        /// The wrapper-allocated redirected-event flag, if any (see
        /// [`Self::Opening`]).
        redirected_event_flag: Option<u16>,
    },
    /// The channel has been revoked by the host.
    Revoked,
}

/// Per-GPADL protocol state.
///
/// Mirrors `vmbus_client`'s internal `GpadlState`, again with runtime
/// handles replaced by [`RequestId`]s so the wrapper can route
/// completions back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpadlPhase {
    /// The client has posted `GpadlHeader` + `GpadlBody` messages and
    /// is waiting for `GpadlCreated`.
    Offered { request_id: RequestId },
    /// The host has acknowledged the GPADL with `GpadlCreated`.
    Created,
    /// The client has posted `GpadlTeardown` and is waiting for
    /// `GpadlTorndown`. `request_ids` is non-empty because multiple
    /// callers can race to tear down the same GPADL; each gets its
    /// own completion when the single `GpadlTorndown` arrives.
    TearingDown {
        request_ids: alloc::vec::Vec<RequestId>,
    },
}

// -- Event flag allocation -------------------------------------------------

/// Errors returned by [`FlagAllocator`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum FlagAllocError {
    /// The flag pool is exhausted ([`FlagAllocator::MAX_FLAGS`] flags
    /// are already allocated).
    #[error("out of event flags")]
    Exhausted,
    /// The requested flag is 0 (reserved for the channel-manager
    /// signal) or out of range.
    #[error("invalid event flag {0}")]
    InvalidFlag(u16),
    /// The requested flag is already allocated (raised by
    /// [`FlagAllocator::reserve`] on a restore path).
    #[error("event flag {0} already in use")]
    AlreadyInUse(u16),
}

/// Manages the pool of synic event flags reserved for channel
/// redirected-interrupt targets.
///
/// Flag ids `1..=2047` can be allocated (flag 0 is reserved for the
/// channel-manager signal). Callers ask for the next free flag via
/// [`Self::allocate`], or reserve a specific one via [`Self::reserve`]
/// on the restore path.
///
/// The wrapper owns the concrete mapping from an allocated flag to a
/// live `pal_event::Event` (on OpenHCL) or in-guest interrupt vector
/// (on opentmk). This allocator only tracks which numeric flag ids are
/// currently in use.
#[derive(Debug, Default)]
pub struct FlagAllocator {
    /// One entry per allocated flag; `true` means the flag is
    /// currently in use. Indexed by `flag_id - 1` (flag 0 is
    /// reserved).
    used: alloc::vec::Vec<bool>,
}

impl FlagAllocator {
    /// The maximum number of concurrent event flags the synic
    /// supports.
    pub const MAX_FLAGS: u16 = 2047;

    /// Allocate the next free flag.
    pub fn allocate(&mut self) -> Result<u16, FlagAllocError> {
        let i = if let Some(i) = self.used.iter().position(|&used| !used) {
            i
        } else if self.used.len() < Self::MAX_FLAGS as usize {
            self.used.push(false);
            self.used.len() - 1
        } else {
            return Err(FlagAllocError::Exhausted);
        };
        self.used[i] = true;
        Ok((i + 1) as u16)
    }

    /// Reserve a specific flag (used on the restore path). Returns
    /// [`FlagAllocError::AlreadyInUse`] if the flag is already
    /// allocated, or [`FlagAllocError::InvalidFlag`] if the flag id
    /// is 0 or exceeds [`Self::MAX_FLAGS`].
    pub fn reserve(&mut self, flag: u16) -> Result<(), FlagAllocError> {
        if flag == 0 || flag > Self::MAX_FLAGS {
            return Err(FlagAllocError::InvalidFlag(flag));
        }
        let i = flag as usize - 1;
        if self.used.len() <= i {
            self.used.resize(i + 1, false);
        }
        if self.used[i] {
            return Err(FlagAllocError::AlreadyInUse(flag));
        }
        self.used[i] = true;
        Ok(())
    }

    /// Free a previously-allocated flag.
    ///
    /// # Panics
    ///
    /// Panics if `flag` is 0 or was not currently allocated. The
    /// caller is expected to only free flags it received from
    /// [`Self::allocate`] or [`Self::reserve`].
    pub fn free(&mut self, flag: u16) {
        assert!(flag != 0 && flag <= Self::MAX_FLAGS);
        let i = flag as usize - 1;
        assert!(i < self.used.len() && self.used[i]);
        self.used[i] = false;
    }

    /// Returns the number of flags currently allocated.
    pub fn used_count(&self) -> usize {
        self.used.iter().filter(|&&u| u).count()
    }
}

#[cfg(test)]
mod flag_alloc_tests {
    use super::*;

    #[test]
    fn allocate_starts_at_1() {
        let mut a = FlagAllocator::default();
        assert_eq!(a.allocate().unwrap(), 1);
        assert_eq!(a.allocate().unwrap(), 2);
    }

    #[test]
    fn free_reuses_slot() {
        let mut a = FlagAllocator::default();
        let a1 = a.allocate().unwrap();
        let a2 = a.allocate().unwrap();
        a.free(a1);
        assert_eq!(a.allocate().unwrap(), a1);
        assert_ne!(a2, a1);
    }

    #[test]
    fn reserve_specific_then_allocate_skips_reserved() {
        let mut a = FlagAllocator::default();
        a.reserve(5).unwrap();
        assert_eq!(a.allocate().unwrap(), 1);
        assert_eq!(a.allocate().unwrap(), 2);
        assert_eq!(a.allocate().unwrap(), 3);
        assert_eq!(a.allocate().unwrap(), 4);
        assert_eq!(a.allocate().unwrap(), 6);
    }

    #[test]
    fn reserve_zero_is_invalid() {
        let mut a = FlagAllocator::default();
        assert_eq!(a.reserve(0), Err(FlagAllocError::InvalidFlag(0)));
    }

    #[test]
    fn double_reserve_fails() {
        let mut a = FlagAllocator::default();
        a.reserve(3).unwrap();
        assert_eq!(a.reserve(3), Err(FlagAllocError::AlreadyInUse(3)));
    }

    #[test]
    fn exhaust_flags() {
        let mut a = FlagAllocator::default();
        for _ in 0..FlagAllocator::MAX_FLAGS {
            a.allocate().unwrap();
        }
        assert_eq!(a.allocate(), Err(FlagAllocError::Exhausted));
    }
}

// -- Wire-adjacent request payloads ----------------------------------------
//
// Parallel to `vmbus_channel::bus`'s OpenData / GpadlRequest /
// ModifyRequest, but without vmbus_channel's std baggage. The wrapper
// converts field-for-field at the Event boundary.

/// Parameters supplied on [`Event::Connect`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ConnectParams {
    /// VP that will service outgoing channel-manager messages.
    pub target_message_vp: u32,
    /// Monitor pages the client offers to the host, or `None` to skip
    /// monitor-page support.
    pub monitor_page: Option<MonitorPageGpas>,
    /// Client identifier the host echoes back in `VersionResponse2`.
    pub client_id: Guid,
}

/// Monitor-page GPAs supplied on [`ConnectParams`] or
/// [`Event::ModifyConnection`]. Wire-equivalent to
/// `vmcore::synic::MonitorPageGpas`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct MonitorPageGpas {
    /// The GPA of the parent-to-child (host → guest) monitor page.
    pub parent_to_child: u64,
    /// The GPA of the child-to-parent (guest → host) monitor page.
    pub child_to_parent: u64,
}

/// Parameters supplied on [`Event::OpenChannel`]. Mirrors
/// `vmbus_channel::bus::OpenData` field-for-field.
#[derive(Copy, Clone, Debug)]
pub struct OpenChannelParams {
    /// Target VP for host-to-guest interrupts, or `None` to disable.
    pub target_vp: Option<u32>,
    /// Byte offset into the ring GPADL where the host-to-guest ring
    /// starts.
    pub ring_offset: u32,
    /// The ring buffer's GPADL id.
    pub ring_gpadl_id: vmbus_core::protocol::GpadlId,
    /// Guest event flag the host signals on empty-to-nonempty
    /// transition.
    pub event_flag: u16,
    /// Connection id for guest-to-host interrupts.
    pub connection_id: u32,
    /// The event flag on the caller-selected pal_event, if the caller
    /// requested a redirected event on this channel. The wrapper owns
    /// the mapping to a real Event handle; the core only tracks the
    /// numeric flag id.
    pub redirected_event_flag: Option<u16>,
    /// Opaque per-channel user data.
    pub user_data: vmbus_core::protocol::UserDefinedData,
}

/// Parameters supplied on [`Event::RestoreChannel`].
#[derive(Copy, Clone, Debug)]
pub struct RestoreChannelParams {
    /// Redirected-event flag persisted in saved state.
    pub redirected_event_flag: Option<u16>,
    /// Connection id persisted in saved state.
    pub connection_id: u32,
}

/// A GPADL description supplied on [`Event::EstablishGpadl`]. Mirrors
/// `vmbus_channel::bus::GpadlRequest` with an owned buffer.
#[derive(Clone, Debug)]
pub struct GpadlRequest {
    /// Fresh gpadl id the caller wants to associate with this GPADL.
    pub id: vmbus_core::protocol::GpadlId,
    /// Number of ranges in the GPADL.
    pub count: u16,
    /// The GPA range buffer (packed per the vmbus spec).
    pub buf: alloc::vec::Vec<u64>,
}

/// A caller-initiated channel modification. Wire-equivalent to
/// `vmbus_channel::bus::ModifyRequest`.
#[derive(Copy, Clone, Debug)]
pub enum ModifyRequest {
    /// Change the target VP for host-to-guest interrupts.
    TargetVp { target_vp: u32 },
}

/// Parameters supplied on [`Event::HvsockConnect`]. Wire-equivalent to
/// `vmbus_core::HvsockConnectRequest`.
#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
pub struct HvsockConnectRequest {
    /// Service id (hvsock connection endpoint).
    pub service_id: Guid,
    /// Endpoint id.
    pub endpoint_id: Guid,
    /// Silo id (`Guid::default()` when not in a silo).
    pub silo_id: Guid,
    /// Whether the client should be treated as silo-unaware on hosts
    /// that don't support silo-aware hvsock.
    pub hosted_silo_unaware: bool,
}

// -- Event: inputs to ClientCore::step -------------------------------------

/// The sole input to `ClientCore::step`.
///
/// Every source of state change the core observes — host wire
/// messages, caller-initiated requests, lifecycle hooks — arrives as
/// an [`Event`]. Deserialisation of host wire bytes happens inside the
/// core.
#[non_exhaustive]
#[derive(Debug)]
pub enum Event<'a> {
    // --- Host-originated ---
    /// A raw vmbus channel-manager message payload arrived on SINT2.
    HostMessage(&'a [u8]),

    // --- Caller-originated top-level requests ---
    /// Post `InitiateContact[2]` and negotiate a version.
    Connect {
        request_id: RequestId,
        params: ConnectParams,
    },
    /// Post `RequestOffers` after a successful connect.
    RequestOffers { request_id: RequestId },
    /// Post `Unload` and disconnect.
    Unload { request_id: RequestId },
    /// Post `ModifyConnection`, changing the monitor pages the host
    /// uses for interrupt aggregation.
    ModifyConnection {
        request_id: RequestId,
        monitor_page: MonitorPageGpas,
    },
    /// Post `TlConnectRequest[2]` and wait for `TlConnectResult`.
    HvsockConnect {
        request_id: RequestId,
        request: HvsockConnectRequest,
    },

    // --- Caller-originated per-channel requests ---
    /// Post `OpenChannel[2]` with the supplied [`OpenChannelParams`].
    OpenChannel {
        request_id: RequestId,
        channel_id: vmbus_core::protocol::ChannelId,
        open: OpenChannelParams,
    },
    /// Restore an [`Opened`](ChannelPhase::Opened) channel from
    /// saved state (no wire message; only updates local state).
    RestoreChannel {
        channel_id: vmbus_core::protocol::ChannelId,
        params: RestoreChannelParams,
    },
    /// Post `CloseChannel` (fire-and-forget).
    CloseChannel {
        channel_id: vmbus_core::protocol::ChannelId,
    },
    /// Post `ModifyChannel` and wait for `ModifyChannelResponse`.
    ModifyChannel {
        request_id: RequestId,
        channel_id: vmbus_core::protocol::ChannelId,
        request: ModifyRequest,
    },
    /// Drop caller-side interest in a channel (no wire message).
    /// Emits [`Action::Complete`] once the underlying release
    /// bookkeeping is done.
    ReleaseChannel {
        channel_id: vmbus_core::protocol::ChannelId,
    },

    // --- Caller-originated per-gpadl requests ---
    /// Post `GpadlHeader` + `GpadlBody` messages and wait for
    /// `GpadlCreated`.
    EstablishGpadl {
        request_id: RequestId,
        channel_id: vmbus_core::protocol::ChannelId,
        gpadl_id: vmbus_core::protocol::GpadlId,
        request: GpadlRequest,
    },
    /// Post `GpadlTeardown` and wait for `GpadlTorndown`.
    TeardownGpadl {
        request_id: RequestId,
        channel_id: vmbus_core::protocol::ChannelId,
        gpadl_id: vmbus_core::protocol::GpadlId,
    },

    // --- Task-level lifecycle hooks ---
    /// Enable request processing (drives the core out of the initial
    /// paused state).
    Start,
    /// Pause new request processing while still draining outstanding
    /// completions.
    Stop,
    /// Post `Pause` (V5+ pause/resume protocol).
    Pause,
    /// Post `Resume`.
    Resume,
    /// Reset all internal state (used only on shutdown paths).
    Reset,
    /// Backpressure hint from the wrapper: `true` means the outgoing
    /// PostMessage retry loop is running and the core should stop
    /// processing caller-initiated requests until it drains.
    HostBusy { busy: bool },
}

// -- Action: outputs from ClientCore::step ---------------------------------

/// A descriptor for a channel offer forwarded through
/// [`Action::OfferReceived`].
#[derive(Clone, Debug)]
pub struct OfferDescriptor {
    /// The raw `OfferChannel` wire message.
    pub offer: vmbus_core::protocol::OfferChannel,
    /// Connection id for guest-to-host interrupts on this channel.
    pub connection_id: u32,
}

/// A completed request result payload delivered on
/// [`Action::Complete`].
#[derive(Debug)]
#[non_exhaustive]
pub enum CompletionResult {
    /// Result of [`Event::Connect`].
    Connect(Result<ConnectionSuccess, ConnectError>),
    /// Result of [`Event::RequestOffers`].
    RequestOffers(Result<(), ConnectError>),
    /// Result of [`Event::Unload`].
    Unload,
    /// Result of [`Event::ModifyConnection`].
    ModifyConnection(vmbus_core::protocol::ConnectionState),
    /// Result of [`Event::HvsockConnect`]. `None` means the host
    /// refused the connection.
    HvsockConnect(Option<OfferDescriptor>),
    /// Result of [`Event::OpenChannel`].
    OpenChannel(Result<u32, i32>),
    /// Result of [`Event::ModifyChannel`].
    ModifyChannel(i32),
    /// Result of [`Event::EstablishGpadl`].
    EstablishGpadl(Result<(), ()>),
    /// Result of [`Event::TeardownGpadl`].
    TeardownGpadl,
    /// Result of [`Event::ReleaseChannel`].
    ReleaseChannel,
}

/// Successful [`Event::Connect`] result — the negotiated
/// [`VersionInfo`] and whether offers were requested implicitly.
#[derive(Copy, Clone, Debug)]
pub struct ConnectionSuccess {
    /// Negotiated version info.
    pub version: VersionInfo,
}

/// Reasons a connect can fail.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConnectError {
    /// The client was not in the `Disconnected` phase.
    #[error("invalid client state for InitiateContact")]
    InvalidState,
    /// The host rejected all versions the client offered.
    #[error("host does not support any of the client's protocol versions")]
    VersionNotSupported,
    /// The host accepted the version but failed the connection with
    /// the enclosed [`vmbus_core::protocol::ConnectionState`] code.
    #[error("host failed the connection with status {0:?}")]
    FailedToConnect(vmbus_core::protocol::ConnectionState),
}

/// Observable per-channel transition delivered via
/// [`Action::ChannelObservable`]. Used by the wrapper for its
/// per-channel `Arc<AtomicU32>` connection-id bookkeeping and for
/// forwarding to `revoke_send` on rescinds.
#[derive(Copy, Clone, Debug)]
#[non_exhaustive]
pub enum ChannelObservable {
    /// A live connection id was assigned to the channel by
    /// `OpenResult`.
    ConnectionIdAssigned(u32),
    /// The channel connection id was cleared by `CloseChannel` or
    /// `RelIdReleased`.
    ConnectionIdCleared,
    /// The channel entered [`ChannelPhase::Opened`].
    Opened,
    /// The channel entered [`ChannelPhase::Offered`] (returned from
    /// Opened after `CloseChannel`).
    Closed,
    /// The channel was revoked by the host.
    Revoked,
}

/// The sole output from `ClientCore::step`.
///
/// Actions are streamed through an [`ActionSink`] in the order the
/// core generates them; the wrapper is expected to observe them
/// synchronously within a single `step` call.
#[non_exhaustive]
#[derive(Debug)]
pub enum Action {
    /// Post a wire-encoded vmbus channel-manager message to the host.
    /// The wrapper handles buffering, retry, and (via
    /// `Event::HostBusy`) backpressure.
    PostMessage(alloc::vec::Vec<u8>),
    /// Signal a guest-to-host event on `(connection_id,
    /// event_flag)`.
    SignalEvent { connection_id: u32, event_flag: u16 },
    /// Free a previously-emitted event flag. The wrapper reclaims the
    /// backing `pal_event::Event` handle.
    FreeEventFlag(u16),
    /// A caller-initiated request has completed.
    Complete {
        request_id: RequestId,
        result: CompletionResult,
    },
    /// The host delivered an offer.
    OfferReceived(OfferDescriptor),
    /// The host rescinded a prior offer.
    OfferRescinded {
        channel_id: vmbus_core::protocol::ChannelId,
    },
    /// A per-channel state observable transition.
    ChannelObservable {
        channel_id: vmbus_core::protocol::ChannelId,
        event: ChannelObservable,
    },
}

// -- ActionSink ------------------------------------------------------------

/// The sink through which [`Action`]s are emitted during a call to
/// `ClientCore::step` (in phase 4b).
///
/// Implementations are typically small structs that own the wrapper's
/// mesh senders or in-guest queues, and dispatch each variant to the
/// appropriate downstream target.
pub trait ActionSink {
    fn emit(&mut self, action: Action);
}

// -- ClientCore skeleton ---------------------------------------------------

/// The state-machine core of the VMBus client.
///
/// Constructed with [`Self::new`] and driven by the wrapper through
/// repeated `step` calls. Deterministic and single-threaded
/// on any runtime — the wrapper is responsible for serialising events
/// (typically via its `select!` loop).
///
/// # Phase-4a state
///
/// This commit lands the crate's public API surface (event / action
/// payload types, the [`Event`] and [`Action`] enums, [`ActionSink`]
/// trait, and [`Self::new`] with the empty initial state). The
/// `step` method arrives in phase 4b along with the message
/// dispatch logic ported from `vmbus_client`'s `handle_*` methods.
#[derive(Debug)]
pub struct ClientCore {
    config: Config,
    phase: ClientPhase,
    channels: alloc::collections::BTreeMap<vmbus_core::protocol::ChannelId, ChannelEntry>,
    outstanding: alloc::collections::BTreeMap<RequestId, PendingRequest>,
    hvsock_pending: alloc::collections::BTreeMap<Guid, RequestId>,
    flag_allocator: FlagAllocator,
    running: bool,
    host_busy: bool,
    /// Set when a `ModifyConnection` is outstanding, so a duplicate
    /// request can be rejected.
    modify_connection_request_id: Option<RequestId>,
}

/// Per-channel entry held in [`ClientCore::channels`].
#[derive(Debug, Clone)]
pub struct ChannelEntry {
    /// Cached copy of the host's `OfferChannel` message.
    pub offer: vmbus_core::protocol::OfferChannel,
    /// Current per-channel state.
    pub phase: ChannelPhase,
    /// Live connection id, or 0 when the channel is offered/revoked.
    pub connection_id: u32,
    /// GPADLs currently associated with this channel.
    pub gpadls: alloc::collections::BTreeMap<vmbus_core::protocol::GpadlId, GpadlPhase>,
    /// `true` once the caller has released their side of the channel.
    /// The core defers actually removing the entry from
    /// [`ClientCore::channels`] until the host acknowledges with
    /// `RelIdReleased`.
    pub is_client_released: bool,
    /// [`RequestId`] of a currently-outstanding `ModifyChannel`, if
    /// any.
    pub modify_request_id: Option<RequestId>,
}

/// A caller-initiated request the core is waiting for the host to
/// complete. Recorded in `ClientCore` so
/// [`Action::Complete`] can be routed back to the wrapper's Rpc.
#[derive(Copy, Clone, Debug)]
pub enum PendingRequest {
    Connect,
    RequestOffers,
    Unload,
    ModifyConnection,
    HvsockConnect {
        service_id: Guid,
    },
    OpenChannel {
        channel_id: vmbus_core::protocol::ChannelId,
    },
    ModifyChannel {
        channel_id: vmbus_core::protocol::ChannelId,
    },
    EstablishGpadl {
        channel_id: vmbus_core::protocol::ChannelId,
        gpadl_id: vmbus_core::protocol::GpadlId,
    },
    TeardownGpadl {
        channel_id: vmbus_core::protocol::ChannelId,
        gpadl_id: vmbus_core::protocol::GpadlId,
    },
    ReleaseChannel {
        channel_id: vmbus_core::protocol::ChannelId,
    },
}

impl ClientCore {
    /// Construct a fresh [`ClientCore`] with the given [`Config`].
    pub fn new(config: Config) -> Self {
        Self {
            config,
            phase: ClientPhase::Disconnected,
            channels: alloc::collections::BTreeMap::new(),
            outstanding: alloc::collections::BTreeMap::new(),
            hvsock_pending: alloc::collections::BTreeMap::new(),
            flag_allocator: FlagAllocator::default(),
            running: false,
            host_busy: false,
            modify_connection_request_id: None,
        }
    }

    /// Returns a reference to the immutable configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Returns the current [`ClientPhase`].
    pub fn phase(&self) -> &ClientPhase {
        &self.phase
    }

    /// Returns the current map of known channels.
    pub fn channels(
        &self,
    ) -> &alloc::collections::BTreeMap<vmbus_core::protocol::ChannelId, ChannelEntry> {
        &self.channels
    }

    /// Whether the wrapper has signalled outgoing-message
    /// backpressure via [`Event::HostBusy`].
    pub fn host_busy(&self) -> bool {
        self.host_busy
    }

    /// Whether request processing is enabled (i.e.
    /// [`Event::Start`] has been observed and [`Event::Stop`] has
    /// not).
    pub fn running(&self) -> bool {
        self.running
    }

    /// Feed one input event to the state machine. All resulting
    /// [`Action`]s are emitted through `sink` in order.
    ///
    /// Deterministic and single-threaded. The wrapper is responsible
    /// for serialising events (typically via its `select!` loop).
    ///
    /// # Phase 4b-i coverage
    ///
    /// Lifecycle hooks ([`Event::Start`] / [`Event::Stop`] /
    /// [`Event::HostBusy`] / [`Event::Reset`]), the top-level dispatch
    /// switch, and safe decoding of [`Event::HostMessage`] via
    /// [`vmbus_core::protocol::Message::parse`]. Phase 4b-ii adds
    /// [`Event::Connect`] and the version-response ladder; later
    /// stages fill in offers, per-channel, and per-gpadl handling.
    pub fn step(&mut self, event: Event<'_>, sink: &mut dyn ActionSink) {
        match event {
            Event::Start => {
                self.running = true;
            }
            Event::Stop => {
                self.running = false;
            }
            Event::Reset => {
                self.phase = ClientPhase::Disconnected;
                self.channels.clear();
                self.outstanding.clear();
                self.hvsock_pending.clear();
                self.flag_allocator = FlagAllocator::default();
                self.host_busy = false;
                self.modify_connection_request_id = None;
                // `running` is deliberately preserved across reset —
                // the wrapper toggles it explicitly via Start/Stop.
            }
            Event::HostBusy { busy } => {
                self.host_busy = busy;
            }
            Event::HostMessage(bytes) => {
                self.dispatch_host_message(bytes, sink);
            }
            Event::Connect { request_id, params } => {
                self.handle_connect(request_id, params, sink);
            }
            // Pause/Resume are wire-only in the V5+ pause-resume
            // protocol; the wrapper generates them by posting the
            // corresponding messages directly. They arrive here as
            // hints only (no protocol-level state transitions).
            Event::Pause => {
                // Placeholder — phase 4b-v (Pause/Resume) will emit
                // the wire message and gate outbound request
                // processing until PauseResponse.
            }
            Event::Resume => {
                // Placeholder — phase 4b-v.
            }
            // Caller-initiated requests, offer channel operations, and
            // gpadl operations land in phase 4b-iii..v. Recorded here
            // as unimplemented to keep the match exhaustive.
            Event::RequestOffers { .. }
            | Event::Unload { .. }
            | Event::ModifyConnection { .. }
            | Event::HvsockConnect { .. }
            | Event::OpenChannel { .. }
            | Event::RestoreChannel { .. }
            | Event::CloseChannel { .. }
            | Event::ModifyChannel { .. }
            | Event::ReleaseChannel { .. }
            | Event::EstablishGpadl { .. }
            | Event::TeardownGpadl { .. } => {
                // Not yet implemented in this phase. See doc comment
                // on `step` for the phased rollout.
            }
        }
    }

    /// Decode and dispatch a host wire message.
    ///
    /// Parse errors are silently dropped: they represent malformed
    /// input from the host, and the client never trusts host framing
    /// enough to panic. Wrapper-side tracing can pick up the raw
    /// bytes at the transport layer if diagnostics are needed.
    fn dispatch_host_message(&mut self, bytes: &[u8], sink: &mut dyn ActionSink) {
        use vmbus_core::protocol::Message;

        let version = self.phase.version();
        let Ok(msg) = Message::parse(bytes, version) else {
            // Malformed host input. Never panic on host framing;
            // wrapper-side logs at the transport layer can pick up
            // the raw bytes if diagnostics are needed.
            return;
        };
        match msg {
            Message::VersionResponse3(v, ..) => {
                self.handle_version_response(v.version_response2, sink);
            }
            Message::VersionResponse2(v, ..) => {
                self.handle_version_response(v, sink);
            }
            Message::VersionResponse(v, ..) => {
                self.handle_version_response(v.into(), sink);
            }
            // Phase 4b-iii and later add offer / gpadl / channel /
            // hvsock / modify / unload handlers. Silently ignore any
            // stale-phase deliveries in the meantime.
            _ => {}
        }
    }

    /// Encode a vmbus channel-manager message and emit an
    /// [`Action::PostMessage`] carrying its wire bytes.
    fn post_message<T>(&self, message: &T, sink: &mut dyn ActionSink)
    where
        T: zerocopy::IntoBytes
            + zerocopy::Immutable
            + zerocopy::KnownLayout
            + vmbus_core::protocol::VmbusMessage,
    {
        let outgoing = vmbus_core::OutgoingMessage::new(message);
        sink.emit(Action::PostMessage(outgoing.data().to_vec()));
    }

    /// Post `InitiateContact` (V1) or `InitiateContact2` (V5+ Copper)
    /// for the given `version`, reusing the caller-supplied
    /// [`ConnectParams`]. Called by [`Self::handle_connect`] and by
    /// [`Self::handle_version_response`] when walking the fallthrough
    /// ladder.
    fn send_initiate_contact(
        &self,
        version: Version,
        params: &ConnectParams,
        sink: &mut dyn ActionSink,
    ) {
        let feature_flags = if version >= Version::Copper {
            self.config.supported_feature_flags
        } else {
            FeatureFlags::new()
        };
        let target_info = vmbus_core::protocol::TargetInfo::new()
            .with_sint(self.config.sint)
            .with_vtl(self.config.vtl)
            .with_feature_flags(feature_flags.into());
        let monitor_page = params.monitor_page.unwrap_or_default();
        let msg = vmbus_core::protocol::InitiateContact2 {
            initiate_contact: vmbus_core::protocol::InitiateContact {
                version_requested: version as u32,
                target_message_vp: params.target_message_vp,
                interrupt_page_or_target_info: target_info.into(),
                parent_to_child_monitor_page_gpa: monitor_page.parent_to_child,
                child_to_parent_monitor_page_gpa: monitor_page.child_to_parent,
            },
            client_id: params.client_id,
        };
        if version < Version::Copper {
            self.post_message(&msg.initiate_contact, sink);
        } else {
            self.post_message(&msg, sink);
        }
    }

    /// Handle [`Event::Connect`] — validate current phase, transition
    /// to `Connecting`, and post the initial `InitiateContact[2]`
    /// with the highest supported version.
    fn handle_connect(
        &mut self,
        request_id: RequestId,
        params: ConnectParams,
        sink: &mut dyn ActionSink,
    ) {
        if !matches!(self.phase, ClientPhase::Disconnected) {
            sink.emit(Action::Complete {
                request_id,
                result: CompletionResult::Connect(Err(ConnectError::InvalidState)),
            });
            return;
        }
        let Some(&version) = self.config.supported_versions.last() else {
            // A misconfigured client (empty supported_versions) can
            // never negotiate. Treat as VersionNotSupported so the
            // caller sees a clean error rather than a hang.
            sink.emit(Action::Complete {
                request_id,
                result: CompletionResult::Connect(Err(ConnectError::VersionNotSupported)),
            });
            return;
        };
        self.outstanding.insert(request_id, PendingRequest::Connect);
        self.phase = ClientPhase::Connecting {
            version,
            request_id,
            params,
        };
        self.send_initiate_contact(version, &params, sink);
    }

    /// Handle a `VersionResponse[2|3]` — either finish the caller's
    /// [`Event::Connect`] or walk down the negotiation ladder.
    fn handle_version_response(
        &mut self,
        msg: vmbus_core::protocol::VersionResponse2,
        sink: &mut dyn ActionSink,
    ) {
        // Take current Connecting phase; leaving Disconnected as the
        // interim value means a stale response can't corrupt state.
        let old_phase = core::mem::replace(&mut self.phase, ClientPhase::Disconnected);
        let ClientPhase::Connecting {
            version,
            request_id,
            params,
        } = old_phase
        else {
            // Stale response — restore the previous phase and drop
            // the message.
            self.phase = old_phase;
            return;
        };

        if msg.version_response.version_supported > 0 {
            if msg.version_response.connection_state
                != vmbus_core::protocol::ConnectionState::SUCCESSFUL
            {
                self.outstanding.remove(&request_id);
                sink.emit(Action::Complete {
                    request_id,
                    result: CompletionResult::Connect(Err(ConnectError::FailedToConnect(
                        msg.version_response.connection_state,
                    ))),
                });
                return;
            }
            let feature_flags = if version >= Version::Copper {
                FeatureFlags::from(msg.supported_features) & self.config.supported_feature_flags
            } else {
                FeatureFlags::new()
            };
            let version_info = VersionInfo {
                version,
                feature_flags,
            };
            self.phase = ClientPhase::Connected {
                version: version_info,
            };
            self.outstanding.remove(&request_id);
            sink.emit(Action::Complete {
                request_id,
                result: CompletionResult::Connect(Ok(ConnectionSuccess {
                    version: version_info,
                })),
            });
            return;
        }

        // Host does not support this version — walk the ladder.
        let versions = self.config.supported_versions;
        let index = match versions.iter().position(|v| *v == version) {
            Some(i) => i,
            None => {
                // Bug — should not happen. Fail cleanly.
                self.outstanding.remove(&request_id);
                sink.emit(Action::Complete {
                    request_id,
                    result: CompletionResult::Connect(Err(ConnectError::VersionNotSupported)),
                });
                return;
            }
        };
        if index == 0 {
            self.outstanding.remove(&request_id);
            sink.emit(Action::Complete {
                request_id,
                result: CompletionResult::Connect(Err(ConnectError::VersionNotSupported)),
            });
            return;
        }
        let next_version = versions[index - 1];
        self.phase = ClientPhase::Connecting {
            version: next_version,
            request_id,
            params,
        };
        self.send_initiate_contact(next_version, &params, sink);
    }
}

#[cfg(test)]
mod step_tests {
    extern crate std;

    use super::*;
    use alloc::vec::Vec;

    /// Scripted [`ActionSink`] used by the unit tests: records every
    /// emitted [`Action`] into a `Vec` so assertions can inspect the
    /// exact sequence.
    #[derive(Default)]
    struct Recording {
        actions: Vec<Action>,
    }

    impl ActionSink for Recording {
        fn emit(&mut self, action: Action) {
            self.actions.push(action);
        }
    }

    fn make_config() -> Config {
        Config {
            sint: vmbus_core::VMBUS_SINT,
            vtl: 0,
            supported_versions: &[Version::Copper],
            supported_feature_flags: FeatureFlags::new(),
        }
    }

    #[test]
    fn new_starts_disconnected_and_not_running() {
        let core = ClientCore::new(make_config());
        assert!(matches!(core.phase(), ClientPhase::Disconnected));
        assert!(!core.running());
        assert!(!core.host_busy());
        assert!(core.channels().is_empty());
    }

    #[test]
    fn start_toggles_running() {
        let mut core = ClientCore::new(make_config());
        let mut sink = Recording::default();
        core.step(Event::Start, &mut sink);
        assert!(core.running());
        assert!(sink.actions.is_empty());
    }

    #[test]
    fn stop_clears_running() {
        let mut core = ClientCore::new(make_config());
        let mut sink = Recording::default();
        core.step(Event::Start, &mut sink);
        core.step(Event::Stop, &mut sink);
        assert!(!core.running());
    }

    #[test]
    fn host_busy_records_backpressure() {
        let mut core = ClientCore::new(make_config());
        let mut sink = Recording::default();
        core.step(Event::HostBusy { busy: true }, &mut sink);
        assert!(core.host_busy());
        core.step(Event::HostBusy { busy: false }, &mut sink);
        assert!(!core.host_busy());
    }

    #[test]
    fn reset_returns_to_initial_state_but_preserves_running() {
        let mut core = ClientCore::new(make_config());
        let mut sink = Recording::default();
        core.step(Event::Start, &mut sink);
        core.step(Event::HostBusy { busy: true }, &mut sink);
        core.step(Event::Reset, &mut sink);
        assert!(matches!(core.phase(), ClientPhase::Disconnected));
        assert!(!core.host_busy());
        // Reset deliberately preserves running (see doc comment on
        // Event::Reset handling in `step`).
        assert!(core.running());
    }

    #[test]
    fn host_message_with_truncated_header_does_not_panic() {
        // Regression guard: any parse failure on host bytes must be
        // swallowed, not panic. VMBus is a trust boundary.
        let mut core = ClientCore::new(make_config());
        let mut sink = Recording::default();
        core.step(Event::HostMessage(&[0u8; 2]), &mut sink);
        core.step(Event::HostMessage(&[]), &mut sink);
        // Any garbage.
        core.step(Event::HostMessage(&[0xff; 64]), &mut sink);
        assert!(sink.actions.is_empty());
        assert!(matches!(core.phase(), ClientPhase::Disconnected));
    }

    // -- Phase 4b-ii: Connect + version-response ladder --------------

    /// Multi-version fixture so the fallthrough ladder can be
    /// exercised. Ordered lowest → highest, matching the workspace
    /// convention that `.last()` is the preferred (highest) version.
    fn make_multi_version_config() -> Config {
        Config {
            sint: vmbus_core::VMBUS_SINT,
            vtl: 0,
            supported_versions: &[Version::Iron, Version::Copper],
            supported_feature_flags: FeatureFlags::new().with_modify_connection(true),
        }
    }

    fn connect_params() -> ConnectParams {
        ConnectParams {
            target_message_vp: 0,
            monitor_page: None,
            client_id: Guid::ZERO,
        }
    }

    /// Wrap a wire-typed message in the `MessageHeader` framing the
    /// core's `dispatch_host_message` expects.
    fn make_host_message<T>(msg: &T) -> Vec<u8>
    where
        T: zerocopy::IntoBytes
            + zerocopy::Immutable
            + zerocopy::KnownLayout
            + vmbus_core::protocol::VmbusMessage,
    {
        vmbus_core::OutgoingMessage::new(msg).data().to_vec()
    }

    /// Extract wire bytes from a captured `Action::PostMessage`.
    fn expect_post(action: &Action) -> &[u8] {
        match action {
            Action::PostMessage(bytes) => bytes.as_slice(),
            other => panic!("expected PostMessage, got {other:?}"),
        }
    }

    #[test]
    fn connect_posts_initiate_contact_and_transitions_to_connecting() {
        let mut core = ClientCore::new(make_config());
        let mut sink = Recording::default();
        core.step(
            Event::Connect {
                request_id: RequestId(7),
                params: connect_params(),
            },
            &mut sink,
        );
        // Exactly one PostMessage; no Complete yet.
        assert_eq!(sink.actions.len(), 1);
        let bytes = expect_post(&sink.actions[0]);
        // The wire message must at least contain the message header
        // and one InitiateContact2 struct.
        assert!(bytes.len() >= 8);
        // Phase transitioned; RequestId is tracked as outstanding.
        assert!(matches!(
            core.phase(),
            ClientPhase::Connecting {
                version: Version::Copper,
                request_id: RequestId(7),
                ..
            }
        ));
    }

    #[test]
    fn connect_from_non_disconnected_phase_completes_with_invalid_state() {
        let mut core = ClientCore::new(make_config());
        let mut sink = Recording::default();
        // First connect: legitimate.
        core.step(
            Event::Connect {
                request_id: RequestId(1),
                params: connect_params(),
            },
            &mut sink,
        );
        sink.actions.clear();
        // Second connect while still Connecting: rejected.
        core.step(
            Event::Connect {
                request_id: RequestId(2),
                params: connect_params(),
            },
            &mut sink,
        );
        assert_eq!(sink.actions.len(), 1);
        assert!(matches!(
            &sink.actions[0],
            Action::Complete {
                request_id: RequestId(2),
                result: CompletionResult::Connect(Err(ConnectError::InvalidState)),
            }
        ));
    }

    #[test]
    fn successful_version_response_transitions_to_connected() {
        let mut core = ClientCore::new(make_multi_version_config());
        let mut sink = Recording::default();
        core.step(
            Event::Connect {
                request_id: RequestId(1),
                params: connect_params(),
            },
            &mut sink,
        );
        sink.actions.clear();

        // Fake a successful VersionResponse2 from the host.
        let response = vmbus_core::protocol::VersionResponse2 {
            version_response: vmbus_core::protocol::VersionResponse {
                version_supported: 1,
                connection_state: vmbus_core::protocol::ConnectionState::SUCCESSFUL,
                padding: 0,
                selected_version_or_connection_id: 1,
            },
            // Modify-connection is in Config; the host offers all
            // Copper flags. Intersection selects modify_connection.
            supported_features: FeatureFlags::new().with_modify_connection(true).into(),
        };
        let wire = make_host_message(&response);
        core.step(Event::HostMessage(&wire), &mut sink);

        // Exactly one Complete, no further posts (phase 4b-ii scope).
        assert_eq!(sink.actions.len(), 1);
        let Action::Complete {
            request_id: RequestId(1),
            result: CompletionResult::Connect(Ok(ConnectionSuccess { version })),
        } = sink.actions[0]
        else {
            panic!("unexpected: {:?}", sink.actions[0]);
        };
        assert_eq!(version.version, Version::Copper);
        assert!(version.feature_flags.modify_connection());
        assert!(matches!(core.phase(), ClientPhase::Connected { .. }));
    }

    #[test]
    fn feature_flags_are_intersected_with_config() {
        // Config only supports modify_connection; host offers every
        // Copper flag. Result must be exactly modify_connection.
        let mut core = ClientCore::new(make_multi_version_config());
        let mut sink = Recording::default();
        core.step(
            Event::Connect {
                request_id: RequestId(1),
                params: connect_params(),
            },
            &mut sink,
        );
        sink.actions.clear();
        let response = vmbus_core::protocol::VersionResponse2 {
            version_response: vmbus_core::protocol::VersionResponse {
                version_supported: 1,
                connection_state: vmbus_core::protocol::ConnectionState::SUCCESSFUL,
                padding: 0,
                selected_version_or_connection_id: 1,
            },
            supported_features: 0xffff_ffff,
        };
        let wire = make_host_message(&response);
        core.step(Event::HostMessage(&wire), &mut sink);
        let ClientPhase::Connected { version } = *core.phase() else {
            panic!();
        };
        assert!(version.feature_flags.modify_connection());
        // Anything not in the config must be cleared.
        assert!(!version.feature_flags.guest_specified_signal_parameters());
    }

    #[test]
    fn host_reject_walks_ladder_and_reposts() {
        let mut core = ClientCore::new(make_multi_version_config());
        let mut sink = Recording::default();
        core.step(
            Event::Connect {
                request_id: RequestId(1),
                params: connect_params(),
            },
            &mut sink,
        );
        // Initial PostMessage for Copper.
        assert_eq!(sink.actions.len(), 1);
        sink.actions.clear();

        // Host says version not supported (v_supported = 0).
        let response = vmbus_core::protocol::VersionResponse2 {
            version_response: vmbus_core::protocol::VersionResponse {
                version_supported: 0,
                connection_state: vmbus_core::protocol::ConnectionState::SUCCESSFUL,
                padding: 0,
                selected_version_or_connection_id: 0,
            },
            supported_features: 0,
        };
        let wire = make_host_message(&response);
        core.step(Event::HostMessage(&wire), &mut sink);

        // Ladder walks to Iron: one more PostMessage, no Complete.
        assert_eq!(sink.actions.len(), 1);
        let _ = expect_post(&sink.actions[0]);
        assert!(matches!(
            core.phase(),
            ClientPhase::Connecting {
                version: Version::Iron,
                ..
            }
        ));
    }

    #[test]
    fn ladder_bottoms_out_with_version_not_supported() {
        let mut core = ClientCore::new(make_config()); // Only Copper.
        let mut sink = Recording::default();
        core.step(
            Event::Connect {
                request_id: RequestId(3),
                params: connect_params(),
            },
            &mut sink,
        );
        sink.actions.clear();

        // Host rejects the only version.
        let response = vmbus_core::protocol::VersionResponse2 {
            version_response: vmbus_core::protocol::VersionResponse {
                version_supported: 0,
                connection_state: vmbus_core::protocol::ConnectionState::SUCCESSFUL,
                padding: 0,
                selected_version_or_connection_id: 0,
            },
            supported_features: 0,
        };
        let wire = make_host_message(&response);
        core.step(Event::HostMessage(&wire), &mut sink);

        assert_eq!(sink.actions.len(), 1);
        assert!(matches!(
            &sink.actions[0],
            Action::Complete {
                request_id: RequestId(3),
                result: CompletionResult::Connect(Err(ConnectError::VersionNotSupported)),
            }
        ));
        assert!(matches!(core.phase(), ClientPhase::Disconnected));
    }

    #[test]
    fn failed_connection_state_completes_with_failed_to_connect() {
        let mut core = ClientCore::new(make_config());
        let mut sink = Recording::default();
        core.step(
            Event::Connect {
                request_id: RequestId(9),
                params: connect_params(),
            },
            &mut sink,
        );
        sink.actions.clear();

        let response = vmbus_core::protocol::VersionResponse2 {
            version_response: vmbus_core::protocol::VersionResponse {
                version_supported: 1,
                connection_state: vmbus_core::protocol::ConnectionState::FAILED_UNKNOWN_FAILURE,
                padding: 0,
                selected_version_or_connection_id: 0,
            },
            supported_features: 0,
        };
        let wire = make_host_message(&response);
        core.step(Event::HostMessage(&wire), &mut sink);

        assert_eq!(sink.actions.len(), 1);
        assert!(matches!(
            &sink.actions[0],
            Action::Complete {
                request_id: RequestId(9),
                result: CompletionResult::Connect(Err(ConnectError::FailedToConnect(_))),
            }
        ));
        assert!(matches!(core.phase(), ClientPhase::Disconnected));
    }

    #[test]
    fn stray_version_response_in_disconnected_phase_is_dropped() {
        // Regression guard: host wire input arriving in the wrong
        // phase must never panic or emit spurious actions.
        let mut core = ClientCore::new(make_config());
        let mut sink = Recording::default();
        let response = vmbus_core::protocol::VersionResponse2 {
            version_response: vmbus_core::protocol::VersionResponse {
                version_supported: 1,
                connection_state: vmbus_core::protocol::ConnectionState::SUCCESSFUL,
                padding: 0,
                selected_version_or_connection_id: 1,
            },
            supported_features: 0,
        };
        let wire = make_host_message(&response);
        core.step(Event::HostMessage(&wire), &mut sink);
        assert!(sink.actions.is_empty());
        assert!(matches!(core.phase(), ClientPhase::Disconnected));
    }
}
