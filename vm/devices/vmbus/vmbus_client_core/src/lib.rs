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
    /// The protocol versions this client will offer, least preferred
    /// first. Negotiation starts at the end of the slice and proceeds
    /// toward the beginning.
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

    fn saved_version(&self) -> Option<VersionInfo> {
        match self {
            Self::Disconnected => None,
            Self::Connected { version } => Some(*version),
            _ => panic!("cannot save while a client request is in flight"),
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
    Offered {
        request_id: RequestId,
        teardown_request_ids: alloc::vec::Vec<RequestId>,
    },
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
        let mut count = 0;
        for &used in &self.used {
            if used {
                count += 1;
            }
        }
        count
    }
}

#[cfg(test)]
mod flag_alloc_tests;

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
        request_id: RequestId,
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
    /// Complete requests that cannot be represented in saved state.
    PrepareSave,
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
    /// Result of [`Event::OpenChannel`] or [`Event::RestoreChannel`].
    OpenChannel(Result<OpenChannelSuccess, OpenChannelError>),
    /// Result of [`Event::ModifyChannel`]. Host-returned NT status.
    ModifyChannel(i32),
    /// Result of [`Event::EstablishGpadl`].
    EstablishGpadl(Result<(), EstablishGpadlError>),
    /// Result of [`Event::TeardownGpadl`].
    TeardownGpadl,
    /// Result of [`Event::ReleaseChannel`].
    ReleaseChannel,
}

/// Successful [`Event::OpenChannel`] or [`Event::RestoreChannel`]
/// result: mirrors `vmbus_client::OpenOutput`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct OpenChannelSuccess {
    /// The wrapper-allocated redirected-event flag, if the caller
    /// requested one on the open.
    pub redirected_event_flag: Option<u16>,
}

/// Reasons an [`Event::OpenChannel`] or [`Event::RestoreChannel`]
/// can fail.
#[derive(Debug, thiserror::Error)]
pub enum OpenChannelError {
    /// The channel is not currently in a state that allows opening
    /// (e.g., already Opened, or in Opening / Restored waiting for
    /// another event).
    #[error("invalid channel state for open")]
    InvalidState,
    /// The channel was revoked by the host before the open completed.
    #[error("channel was revoked by the host")]
    Revoked,
    /// The negotiated protocol version does not support the requested
    /// interrupt-redirection or VTL2 connection-id feature.
    #[error("negotiated protocol version does not support the requested interrupt feature")]
    UnsupportedInterruptFeature,
    /// The host completed `Message::OpenResult` with a non-success
    /// NT status.
    #[error("host reported open-channel status {0:#x}")]
    HostFailed(i32),
}

/// Reasons an [`Event::EstablishGpadl`] can fail.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EstablishGpadlError {
    /// The GPADL payload is too large for the wire format.
    #[error("gpadl request is too large")]
    RequestTooLarge,
    /// The channel is not known to the client.
    #[error("gpadl request references an unknown channel")]
    UnknownChannel,
    /// The channel already has a GPADL with this ID.
    #[error("gpadl ID is already in use")]
    DuplicateId,
    /// The host rejected the GPADL with the supplied status.
    #[error("gpadl creation failed: {0:#x}")]
    HostRejected(i32),
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
    /// The host acknowledged a previously posted [`Event::Pause`].
    PauseComplete,
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
    hvsock_pending:
        alloc::collections::BTreeMap<(Guid, Guid), alloc::collections::VecDeque<RequestId>>,
    teardown_gpadls: alloc::collections::BTreeMap<
        vmbus_core::protocol::GpadlId,
        vmbus_core::protocol::ChannelId,
    >,
    released_channel_ids: alloc::vec::Vec<vmbus_core::protocol::ChannelId>,
    flag_allocator: FlagAllocator,
    running: bool,
    host_busy: bool,
    /// Set when a `ModifyConnection` is outstanding, so a duplicate
    /// request can be rejected.
    modify_connection_request_id: Option<RequestId>,
    malformed_host_message_count: u64,
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

/// Runtime-independent saved state for [`ClientCore`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedState {
    /// Negotiated protocol version, or `None` when disconnected.
    pub version: Option<VersionInfo>,
    /// Channels that can be restored.
    pub channels: alloc::vec::Vec<SavedChannel>,
    /// Revoked channels omitted from `channels` that need a deferred
    /// `RelIdReleased` message after restore.
    pub released_channel_ids: alloc::vec::Vec<vmbus_core::protocol::ChannelId>,
}

/// Saved protocol state for one channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedChannel {
    /// Original host offer.
    pub offer: vmbus_core::protocol::OfferChannel,
    /// Saved channel phase.
    pub phase: SavedChannelPhase,
    /// GPADLs associated with the channel.
    pub gpadls: alloc::vec::Vec<SavedGpadl>,
}

/// Channel phases that are valid at a save boundary.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SavedChannelPhase {
    Offered,
    Opened,
}

/// Saved state for one GPADL.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SavedGpadl {
    pub id: vmbus_core::protocol::GpadlId,
    pub phase: SavedGpadlPhase,
}

/// GPADL phases that are valid at a save boundary.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SavedGpadlPhase {
    Created,
    TearingDown {
        /// Another channel currently owns the in-flight teardown for this ID.
        queued: bool,
    },
}

/// Errors returned while restoring [`SavedState`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RestoreError {
    #[error("unsupported protocol version {0:#x}")]
    UnsupportedVersion(u32),
    #[error("unsupported feature flags {0:#x}")]
    UnsupportedFeatureFlags(u32),
    #[error("duplicate channel id {0}")]
    DuplicateChannelId(u32),
    #[error("duplicate gpadl id {0}")]
    DuplicateGpadlId(u32),
    #[error("queued gpadl teardown {0} has no in-flight owner")]
    QueuedGpadlWithoutOwner(u32),
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
            teardown_gpadls: alloc::collections::BTreeMap::new(),
            released_channel_ids: alloc::vec::Vec::new(),
            flag_allocator: FlagAllocator::default(),
            running: false,
            host_busy: false,
            modify_connection_request_id: None,
            malformed_host_message_count: 0,
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

    /// Number of malformed host messages discarded by this core instance.
    pub fn malformed_host_message_count(&self) -> u64 {
        self.malformed_host_message_count
    }

    /// Number of caller requests waiting for a protocol completion.
    pub fn pending_request_count(&self) -> usize {
        self.outstanding.len()
    }

    /// Number of hvsock connection requests waiting for a host response.
    pub fn pending_hvsock_request_count(&self) -> usize {
        self.hvsock_pending
            .values()
            .map(|requests| requests.len())
            .sum()
    }

    /// Number of GPADL IDs with an active teardown request.
    pub fn pending_gpadl_teardown_count(&self) -> usize {
        self.teardown_gpadls.len()
    }

    /// Number of redirected event flags currently reserved.
    pub fn event_flag_count(&self) -> usize {
        self.flag_allocator.used_count()
    }

    /// Whether a connection modification is waiting for a host response.
    pub fn modify_connection_pending(&self) -> bool {
        self.modify_connection_request_id.is_some()
    }

    /// Allocate a redirected-event flag from the internal pool.
    ///
    /// The wrapper calls this before firing [`Event::OpenChannel`]
    /// (with `redirected_event_flag = Some(flag)`) or
    /// [`Event::RestoreChannel`], and is responsible for registering
    /// its `pal_event::Event` under the returned flag in its own
    /// map. On close, rescind, or failed open, the core emits
    /// [`Action::FreeEventFlag`] after returning the flag to this
    /// pool — the wrapper only needs to drop its
    /// `pal_event::Event` mapping.
    pub fn allocate_event_flag(&mut self) -> Result<u16, FlagAllocError> {
        self.flag_allocator.allocate()
    }

    /// Reserve a specific redirected-event flag while restoring a channel.
    pub fn reserve_event_flag(&mut self, flag: u16) -> Result<(), FlagAllocError> {
        self.flag_allocator.reserve(flag)
    }

    /// Return an allocated redirected-event flag after wrapper-side setup fails.
    pub fn free_event_flag(&mut self, flag: u16) {
        self.flag_allocator.free(flag);
    }

    /// Capture the protocol state at a quiesced save boundary.
    ///
    /// # Panics
    ///
    /// Panics if a caller attempts to save while a protocol request is in
    /// flight. The wrapper must stop and drain the client before saving.
    pub fn save(&self) -> SavedState {
        assert!(
            self.modify_connection_request_id.is_none(),
            "cannot save while a connection modification is in flight"
        );
        assert!(
            self.hvsock_pending.is_empty(),
            "cannot save while an hvsock connection is in flight"
        );
        let version = self.phase.saved_version();
        let (channels, released_channel_ids) = Self::save_channels(
            &self.channels,
            &self.teardown_gpadls,
            self.released_channel_ids.clone(),
        );
        SavedState {
            version,
            channels,
            released_channel_ids,
        }
    }

    fn save_channels(
        entries: &alloc::collections::BTreeMap<vmbus_core::protocol::ChannelId, ChannelEntry>,
        teardown_gpadls: &alloc::collections::BTreeMap<
            vmbus_core::protocol::GpadlId,
            vmbus_core::protocol::ChannelId,
        >,
        mut released_channel_ids: alloc::vec::Vec<vmbus_core::protocol::ChannelId>,
    ) -> (
        alloc::vec::Vec<SavedChannel>,
        alloc::vec::Vec<vmbus_core::protocol::ChannelId>,
    ) {
        let mut channels = alloc::vec::Vec::new();
        for (&channel_id, entry) in entries {
            if let Some(channel) = Self::save_channel(channel_id, entry, teardown_gpadls) {
                channels.push(channel);
            } else {
                released_channel_ids.push(channel_id);
            }
        }
        (channels, released_channel_ids)
    }

    fn save_channel(
        channel_id: vmbus_core::protocol::ChannelId,
        entry: &ChannelEntry,
        teardown_gpadls: &alloc::collections::BTreeMap<
            vmbus_core::protocol::GpadlId,
            vmbus_core::protocol::ChannelId,
        >,
    ) -> Option<SavedChannel> {
        assert!(
            entry.modify_request_id.is_none(),
            "cannot save a channel that is being modified"
        );
        let phase = match entry.phase {
            ChannelPhase::Offered => SavedChannelPhase::Offered,
            ChannelPhase::Restored | ChannelPhase::Opened { .. } => SavedChannelPhase::Opened,
            ChannelPhase::Opening { .. } => {
                panic!("cannot save a channel that is being opened")
            }
            ChannelPhase::Revoked => {
                assert!(
                    entry
                        .gpadls
                        .values()
                        .all(|phase| matches!(phase, GpadlPhase::Created)),
                    "revoked channel has a pending GPADL request"
                );
                return None;
            }
        };
        let mut gpadls = alloc::vec::Vec::with_capacity(entry.gpadls.len());
        for (&id, phase) in &entry.gpadls {
            let phase = match phase {
                GpadlPhase::Created => SavedGpadlPhase::Created,
                GpadlPhase::TearingDown { .. } => SavedGpadlPhase::TearingDown {
                    queued: teardown_gpadls.get(&id) != Some(&channel_id),
                },
                GpadlPhase::Offered { .. } => {
                    panic!("cannot save a GPADL that is being established")
                }
            };
            gpadls.push(SavedGpadl { id, phase });
        }
        Some(SavedChannel {
            offer: entry.offer,
            phase,
            gpadls,
        })
    }

    /// Replace the protocol state with a previously captured snapshot.
    pub fn restore(&mut self, saved: SavedState) -> Result<(), RestoreError> {
        if let Some(version) = saved.version {
            if !self.config.supported_versions.contains(&version.version) {
                return Err(RestoreError::UnsupportedVersion(version.version as u32));
            }
            if !self
                .config
                .supported_feature_flags
                .contains(version.feature_flags)
            {
                return Err(RestoreError::UnsupportedFeatureFlags(
                    version.feature_flags.into(),
                ));
            }
        }

        let connected = saved.version.is_some();
        let saved_channels = if connected {
            saved.channels
        } else {
            alloc::vec::Vec::new()
        };
        let released_channel_ids = if connected {
            saved.released_channel_ids
        } else {
            alloc::vec::Vec::new()
        };
        let mut channels = alloc::collections::BTreeMap::new();
        let mut teardown_gpadls = alloc::collections::BTreeMap::new();
        let mut queued_gpadls = alloc::collections::BTreeSet::new();
        for channel in saved_channels {
            let channel_id = channel.offer.channel_id;
            let gpadls = Self::restore_gpadls(
                channel_id,
                channel.gpadls,
                &mut teardown_gpadls,
                &mut queued_gpadls,
            )?;
            let phase = match channel.phase {
                SavedChannelPhase::Offered => ChannelPhase::Offered,
                SavedChannelPhase::Opened => ChannelPhase::Restored,
            };
            if channels
                .insert(
                    channel_id,
                    ChannelEntry {
                        offer: channel.offer,
                        phase,
                        connection_id: 0,
                        gpadls,
                        is_client_released: false,
                        modify_request_id: None,
                    },
                )
                .is_some()
            {
                return Err(RestoreError::DuplicateChannelId(channel_id.0));
            }
        }
        if let Some(gpadl_id) = queued_gpadls
            .into_iter()
            .find(|gpadl_id| !teardown_gpadls.contains_key(gpadl_id))
        {
            return Err(RestoreError::QueuedGpadlWithoutOwner(gpadl_id.0));
        }

        self.phase = match saved.version {
            Some(version) => ClientPhase::Connected { version },
            None => ClientPhase::Disconnected,
        };
        self.channels = channels;
        self.outstanding.clear();
        self.hvsock_pending.clear();
        self.teardown_gpadls = teardown_gpadls;
        self.released_channel_ids = released_channel_ids;
        self.flag_allocator = FlagAllocator::default();
        self.host_busy = false;
        self.modify_connection_request_id = None;
        Ok(())
    }

    fn restore_gpadls(
        channel_id: vmbus_core::protocol::ChannelId,
        saved: alloc::vec::Vec<SavedGpadl>,
        teardown_gpadls: &mut alloc::collections::BTreeMap<
            vmbus_core::protocol::GpadlId,
            vmbus_core::protocol::ChannelId,
        >,
        queued_gpadls: &mut alloc::collections::BTreeSet<vmbus_core::protocol::GpadlId>,
    ) -> Result<alloc::collections::BTreeMap<vmbus_core::protocol::GpadlId, GpadlPhase>, RestoreError>
    {
        let mut gpadls = alloc::collections::BTreeMap::new();
        for gpadl in saved {
            let phase = match gpadl.phase {
                SavedGpadlPhase::Created => GpadlPhase::Created,
                SavedGpadlPhase::TearingDown { queued } => {
                    if queued {
                        queued_gpadls.insert(gpadl.id);
                    } else if teardown_gpadls.insert(gpadl.id, channel_id).is_some() {
                        return Err(RestoreError::DuplicateGpadlId(gpadl.id.0));
                    }
                    GpadlPhase::TearingDown {
                        request_ids: alloc::vec::Vec::new(),
                    }
                }
            };
            if gpadls.insert(gpadl.id, phase).is_some() {
                return Err(RestoreError::DuplicateGpadlId(gpadl.id.0));
            }
        }
        Ok(gpadls)
    }

    /// Close restored channels that were not reclaimed and tear down their GPADLs.
    pub fn post_restore<S: ActionSink + ?Sized>(&mut self, sink: &mut S) {
        for channel_id in core::mem::take(&mut self.released_channel_ids) {
            Self::post_message(&vmbus_core::protocol::RelIdReleased { channel_id }, sink);
        }

        let restored = self
            .channels
            .iter()
            .filter_map(|(&channel_id, entry)| {
                matches!(entry.phase, ChannelPhase::Restored).then_some(channel_id)
            })
            .collect::<alloc::vec::Vec<_>>();

        for channel_id in restored {
            Self::post_message(&vmbus_core::protocol::CloseChannel { channel_id }, sink);
            let gpadls = {
                let entry = self.channels.get_mut(&channel_id).expect("collected above");
                entry.phase = ChannelPhase::Offered;
                entry
                    .gpadls
                    .iter()
                    .filter_map(|(&gpadl_id, phase)| match phase {
                        GpadlPhase::Created => Some(gpadl_id),
                        GpadlPhase::TearingDown { .. } => None,
                        GpadlPhase::Offered { .. } => {
                            unreachable!("restore never creates offered GPADLs")
                        }
                    })
                    .collect::<alloc::vec::Vec<_>>()
            };
            for gpadl_id in gpadls {
                let entry = self.channels.get_mut(&channel_id).expect("collected above");
                let Some(phase @ GpadlPhase::Created) = entry.gpadls.get_mut(&gpadl_id) else {
                    continue;
                };
                *phase = GpadlPhase::TearingDown {
                    request_ids: alloc::vec::Vec::new(),
                };
                if let alloc::collections::btree_map::Entry::Vacant(entry) =
                    self.teardown_gpadls.entry(gpadl_id)
                {
                    entry.insert(channel_id);
                    Self::post_message(
                        &vmbus_core::protocol::GpadlTeardown {
                            channel_id,
                            gpadl_id,
                        },
                        sink,
                    );
                }
            }
        }
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
    pub fn step<S: ActionSink + ?Sized>(&mut self, event: Event<'_>, sink: &mut S) {
        match event {
            Event::Start => {
                self.running = true;
            }
            Event::Stop => {
                self.running = false;
            }
            Event::PrepareSave => {
                self.handle_prepare_save(sink);
            }
            Event::Reset => {
                self.phase = ClientPhase::Disconnected;
                self.channels.clear();
                self.outstanding.clear();
                self.hvsock_pending.clear();
                self.teardown_gpadls.clear();
                self.released_channel_ids.clear();
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
            Event::RequestOffers { request_id } => {
                self.handle_request_offers(request_id, sink);
            }
            Event::OpenChannel {
                request_id,
                channel_id,
                open,
            } => {
                self.handle_open_channel(request_id, channel_id, open, sink);
            }
            Event::RestoreChannel {
                request_id,
                channel_id,
                params,
            } => {
                self.handle_restore_channel(request_id, channel_id, params, sink);
            }
            Event::CloseChannel { channel_id } => {
                self.handle_close_channel(channel_id, sink);
            }
            Event::ModifyChannel {
                request_id,
                channel_id,
                request,
            } => {
                self.handle_modify_channel(request_id, channel_id, request, sink);
            }
            Event::ReleaseChannel { channel_id } => {
                self.handle_release_channel(channel_id, sink);
            }
            Event::EstablishGpadl {
                request_id,
                channel_id,
                gpadl_id,
                request,
            } => {
                self.handle_establish_gpadl(request_id, channel_id, gpadl_id, request, sink);
            }
            Event::TeardownGpadl {
                request_id,
                channel_id,
                gpadl_id,
            } => {
                self.handle_teardown_gpadl(request_id, channel_id, gpadl_id, sink);
            }
            Event::Unload { request_id } => {
                self.handle_unload(request_id, sink);
            }
            Event::ModifyConnection {
                request_id,
                monitor_page,
            } => {
                self.handle_modify_connection(request_id, monitor_page, sink);
            }
            Event::HvsockConnect {
                request_id,
                request,
            } => {
                self.handle_hvsock_connect(request_id, request, sink);
            }
            Event::Pause => {
                self.handle_pause(sink);
            }
            Event::Resume => {
                self.handle_resume(sink);
            }
        }
    }

    /// Decode and dispatch a host wire message.
    ///
    /// Parse errors are dropped and counted: they represent malformed
    /// input from the host, and the client never trusts host framing
    /// enough to panic.
    fn dispatch_host_message<S: ActionSink + ?Sized>(&mut self, bytes: &[u8], sink: &mut S) {
        use vmbus_core::protocol::Message;

        let version = self.phase.version();
        let Ok(msg) = Message::parse(bytes, version) else {
            // Malformed host input. Never panic on host framing;
            // retain a counter so wrappers can diagnose a stalled
            // request without logging attacker-controlled data.
            self.malformed_host_message_count = self.malformed_host_message_count.saturating_add(1);
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
            Message::OfferChannel(offer, ..) => {
                self.handle_offer(offer, sink);
            }
            Message::AllOffersDelivered(..) => {
                self.handle_offers_delivered(sink);
            }
            Message::RescindChannelOffer(rescind, ..) => {
                self.handle_rescind(rescind, sink);
            }
            Message::OpenResult(result, ..) => {
                self.handle_open_result(result, sink);
            }
            Message::ModifyChannelResponse(response, ..) => {
                self.handle_modify_channel_response(response, sink);
            }
            Message::GpadlCreated(created, ..) => {
                self.handle_gpadl_created(created, sink);
            }
            Message::GpadlTorndown(torndown, ..) => {
                self.handle_gpadl_torndown(torndown, sink);
            }
            Message::UnloadComplete(..) => {
                self.handle_unload_complete(sink);
            }
            Message::ModifyConnectionResponse(response, ..) => {
                self.handle_modify_connection_response(response, sink);
            }
            Message::TlConnectResult(result, ..) => {
                self.handle_tl_connect_result(result, sink);
            }
            Message::PauseResponse(..) => {
                self.handle_pause_response(sink);
            }
            // Silently ignore any other host wire message. Anything
            // that arrives here is either a message this client
            // never sends (e.g., CloseReservedChannelResponse) or a
            // stale-phase delivery. Never panic on host input.
            _ => {}
        }
    }

    /// Encode a vmbus channel-manager message and emit an
    /// [`Action::PostMessage`] carrying its wire bytes.
    fn post_message<T, S: ActionSink + ?Sized>(message: &T, sink: &mut S)
    where
        T: zerocopy::IntoBytes + zerocopy::Immutable + vmbus_core::protocol::VmbusMessage,
    {
        let outgoing = vmbus_core::OutgoingMessage::new(message);
        sink.emit(Action::PostMessage(outgoing.data().to_vec()));
    }

    /// Encode a vmbus channel-manager message with a trailing data
    /// blob (used by GPADL header/body).
    fn post_message_with_data<T, S: ActionSink + ?Sized>(message: &T, data: &[u8], sink: &mut S)
    where
        T: zerocopy::IntoBytes + zerocopy::Immutable + vmbus_core::protocol::VmbusMessage,
    {
        let outgoing = vmbus_core::OutgoingMessage::with_data(message, data);
        sink.emit(Action::PostMessage(outgoing.data().to_vec()));
    }

    /// Post `InitiateContact` (V1) or `InitiateContact2` (V5+ Copper)
    /// for the given `version`, reusing the caller-supplied
    /// [`ConnectParams`]. Called by [`Self::handle_connect`] and by
    /// [`Self::handle_version_response`] when walking the fallthrough
    /// ladder.
    fn send_initiate_contact<S: ActionSink + ?Sized>(
        &self,
        version: Version,
        params: &ConnectParams,
        sink: &mut S,
    ) {
        let msg = Self::initiate_contact_message(
            version,
            *params,
            self.config.sint,
            self.config.vtl,
            self.config.supported_feature_flags,
        );
        if version < Version::Copper {
            Self::post_message(&msg.initiate_contact, sink);
        } else {
            Self::post_message(&msg, sink);
        }
    }

    fn initiate_contact_message(
        version: Version,
        params: ConnectParams,
        sint: u8,
        vtl: u8,
        supported_feature_flags: FeatureFlags,
    ) -> vmbus_core::protocol::InitiateContact2 {
        let feature_flags = if version >= Version::Copper {
            supported_feature_flags
        } else {
            FeatureFlags::new()
        };
        let target_info = vmbus_core::protocol::TargetInfo::new()
            .with_sint(sint)
            .with_vtl(vtl)
            .with_feature_flags(feature_flags.into());
        let monitor_page = params.monitor_page.unwrap_or_default();
        vmbus_core::protocol::InitiateContact2 {
            initiate_contact: vmbus_core::protocol::InitiateContact {
                version_requested: version as u32,
                target_message_vp: params.target_message_vp,
                interrupt_page_or_target_info: target_info.into(),
                parent_to_child_monitor_page_gpa: monitor_page.parent_to_child,
                child_to_parent_monitor_page_gpa: monitor_page.child_to_parent,
            },
            client_id: params.client_id,
        }
    }

    /// Handle [`Event::Connect`] — validate current phase, transition
    /// to `Connecting`, and post the initial `InitiateContact[2]`
    /// with the highest supported version.
    fn handle_connect<S: ActionSink + ?Sized>(
        &mut self,
        request_id: RequestId,
        params: ConnectParams,
        sink: &mut S,
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
    fn handle_version_response<S: ActionSink + ?Sized>(
        &mut self,
        msg: vmbus_core::protocol::VersionResponse2,
        sink: &mut S,
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

    /// Handle [`Event::RequestOffers`] — validate phase, post a
    /// `RequestOffers` message, and transition to `RequestingOffers`.
    fn handle_request_offers<S: ActionSink + ?Sized>(
        &mut self,
        request_id: RequestId,
        sink: &mut S,
    ) {
        let ClientPhase::Connected { version } = *self.phase() else {
            sink.emit(Action::Complete {
                request_id,
                result: CompletionResult::RequestOffers(Err(ConnectError::InvalidState)),
            });
            return;
        };
        self.outstanding
            .insert(request_id, PendingRequest::RequestOffers);
        self.phase = ClientPhase::RequestingOffers {
            version,
            request_id,
            offer_count: 0,
        };
        Self::post_message(&vmbus_core::protocol::RequestOffers {}, sink);
    }

    /// Handle a host-originated `OfferChannel` — record the channel
    /// in the core's map and forward it to the wrapper, either as a
    /// regular offer or as the completion of a pending
    /// [`Event::HvsockConnect`] when the offer matches a tracked
    /// hvsock service.
    fn handle_offer<S: ActionSink + ?Sized>(
        &mut self,
        offer: vmbus_core::protocol::OfferChannel,
        sink: &mut S,
    ) {
        // Duplicate offer for a live channel is a host protocol
        // violation; drop it rather than panicking.
        if self.channels.contains_key(&offer.channel_id) {
            return;
        }
        let hvsock_request_id = self.match_hvsock_offer(&offer);
        if hvsock_request_id.is_none()
            && !matches!(
                self.phase,
                ClientPhase::Connected { .. } | ClientPhase::RequestingOffers { .. }
            )
        {
            return;
        }
        self.channels.insert(
            offer.channel_id,
            ChannelEntry {
                offer,
                phase: ChannelPhase::Offered,
                connection_id: 0,
                gpadls: alloc::collections::BTreeMap::new(),
                is_client_released: false,
                modify_request_id: None,
            },
        );
        // Bump the count so the wrapper's caller-side receiver can
        // reason about the RequestingOffers accumulation length. The
        // count is informational only; the wrapper doesn't need to
        // observe it directly.
        if let ClientPhase::RequestingOffers { offer_count, .. } = &mut self.phase {
            *offer_count = offer_count.saturating_add(1);
        }
        // Hvsock offer check: if the incoming offer matches a
        // pending HvsockConnect, complete that instead of raising a
        // regular OfferReceived. Matches vmbus_client's
        // hvsock_tracker::check_offer semantics.
        if let Some(request_id) = hvsock_request_id {
            self.outstanding.remove(&request_id);
            sink.emit(Action::Complete {
                request_id,
                result: CompletionResult::HvsockConnect(Some(OfferDescriptor {
                    offer,
                    connection_id: 0,
                })),
            });
            return;
        }
        sink.emit(Action::OfferReceived(OfferDescriptor {
            offer,
            connection_id: 0,
        }));
    }

    /// Look up an offered channel against pending hvsock requests.
    /// Returns the [`RequestId`] of the matching pending request if
    /// the offer is an hvsock guest-connect (rather than
    /// host-accept) result.
    fn match_hvsock_offer(
        &mut self,
        offer: &vmbus_core::protocol::OfferChannel,
    ) -> Option<RequestId> {
        if !offer.flags.tlnpi_provider() {
            return None;
        }
        // The wire user_defined blob for tlnpi offers is prefixed
        // with an HvsockUserDefinedParameters struct. If the guest
        // accept flag is nonzero the offer is from another guest
        // asking us to accept, not a response to our connect.
        let params = offer.user_defined.as_hvsock_params();
        if params.is_for_guest_accept != 0 {
            return None;
        }
        // Wrapper's check_offer matches (service_id, endpoint_id)
        // against offer.interface_id + offer.instance_id.
        self.pop_hvsock_request((offer.interface_id, offer.instance_id))
    }

    /// Handle `AllOffersDelivered` — complete the outstanding
    /// [`Event::RequestOffers`] and transition back to
    /// [`ClientPhase::Connected`].
    fn handle_offers_delivered<S: ActionSink + ?Sized>(&mut self, sink: &mut S) {
        let old_phase = core::mem::replace(&mut self.phase, ClientPhase::Disconnected);
        let ClientPhase::RequestingOffers {
            version,
            request_id,
            ..
        } = old_phase
        else {
            self.phase = old_phase;
            return;
        };
        self.phase = ClientPhase::Connected { version };
        self.outstanding.remove(&request_id);
        sink.emit(Action::Complete {
            request_id,
            result: CompletionResult::RequestOffers(Ok(())),
        });
    }

    /// Handle a host-originated `RescindChannelOffer` — mark the
    /// channel as [`ChannelPhase::Revoked`] and forward the rescind
    /// to the wrapper. The wrapper is responsible for issuing
    /// [`Event::ReleaseChannel`] once the caller acknowledges.
    ///
    /// # Phase 4b-iii-a coverage
    ///
    /// Rescind at this phase deals with channels in
    /// [`ChannelPhase::Offered`], [`ChannelPhase::Restored`], and
    /// (idempotently) [`ChannelPhase::Revoked`]. Rescinds that arrive
    /// while a channel is [`ChannelPhase::Opening`] or
    /// [`ChannelPhase::Opened`] gain their pending-open-cancellation
    /// and event-flag-cleanup logic in phase 4b-iii-b, along with
    /// the caller-side `OpenChannel` flow that puts the channel in
    /// those states in the first place.
    fn handle_rescind<S: ActionSink + ?Sized>(
        &mut self,
        rescind: vmbus_core::protocol::RescindChannelOffer,
        sink: &mut S,
    ) {
        let channel_id = rescind.channel_id;
        let Some(entry) = self.channels.get_mut(&channel_id) else {
            // Rescind for an unknown channel: host protocol
            // violation. Drop.
            return;
        };
        // Cancel any state that was mid-flight, then transition to
        // Revoked. The old phase drives what cleanup we need to emit.
        let old_phase = core::mem::replace(&mut entry.phase, ChannelPhase::Revoked);
        entry.connection_id = 0;
        match old_phase {
            ChannelPhase::Offered | ChannelPhase::Restored | ChannelPhase::Revoked => {}
            ChannelPhase::Opening {
                request_id,
                redirected_event_flag,
            } => {
                self.outstanding.remove(&request_id);
                if let Some(flag) = redirected_event_flag {
                    self.free_event_flag_and_notify(flag, sink);
                }
                sink.emit(Action::Complete {
                    request_id,
                    result: CompletionResult::OpenChannel(Err(OpenChannelError::Revoked)),
                });
            }
            ChannelPhase::Opened {
                redirected_event_flag,
            } => {
                if let Some(flag) = redirected_event_flag {
                    self.free_event_flag_and_notify(flag, sink);
                }
            }
        }
        sink.emit(Action::OfferRescinded { channel_id });
        self.try_release_channel(channel_id, sink);
    }

    /// Handle [`Event::OpenChannel`] — validate channel state,
    /// verify feature-flag support for redirection / VTL2 conn id,
    /// post `OpenChannel` or `OpenChannel2`, transition to
    /// [`ChannelPhase::Opening`]. On failure the caller-supplied
    /// (pre-allocated) `redirected_event_flag` is freed via
    /// [`Action::FreeEventFlag`].
    fn handle_open_channel<S: ActionSink + ?Sized>(
        &mut self,
        request_id: RequestId,
        channel_id: vmbus_core::protocol::ChannelId,
        params: OpenChannelParams,
        sink: &mut S,
    ) {
        let Some(entry) = self.channels.get(&channel_id) else {
            self.reject_open(
                request_id,
                params.redirected_event_flag,
                OpenChannelError::InvalidState,
                sink,
            );
            return;
        };
        match entry.phase {
            ChannelPhase::Offered => {}
            ChannelPhase::Revoked => {
                self.reject_open(
                    request_id,
                    params.redirected_event_flag,
                    OpenChannelError::Revoked,
                    sink,
                );
                return;
            }
            _ => {
                self.reject_open(
                    request_id,
                    params.redirected_event_flag,
                    OpenChannelError::InvalidState,
                    sink,
                );
                return;
            }
        }
        let Some(version) = self.phase.version() else {
            self.reject_open(
                request_id,
                params.redirected_event_flag,
                OpenChannelError::InvalidState,
                sink,
            );
            return;
        };
        let supports_redirection = version.feature_flags.guest_specified_signal_parameters()
            || version.feature_flags.channel_interrupt_redirection();
        if params.redirected_event_flag.is_some() && !supports_redirection {
            self.reject_open(
                request_id,
                params.redirected_event_flag,
                OpenChannelError::UnsupportedInterruptFeature,
                sink,
            );
            return;
        }
        // For non-redirection-capable hosts, the wire-level event
        // flag must equal channel_id (see vmbus_client). Otherwise
        // the caller's requested flag can't be honoured.
        if !supports_redirection && params.event_flag != channel_id.0 as u16 {
            self.reject_open(
                request_id,
                params.redirected_event_flag,
                OpenChannelError::UnsupportedInterruptFeature,
                sink,
            );
            return;
        }

        let open_channel = vmbus_core::protocol::OpenChannel {
            channel_id,
            open_id: 0,
            ring_buffer_gpadl_id: params.ring_gpadl_id,
            target_vp: params
                .target_vp
                .unwrap_or(vmbus_core::protocol::VP_INDEX_DISABLE_INTERRUPT),
            downstream_ring_buffer_page_offset: params.ring_offset,
            user_data: params.user_data,
        };
        let mut flags = vmbus_core::protocol::OpenChannelFlags::new();
        if params.redirected_event_flag.is_some() {
            flags.set_redirect_interrupt(true);
        }
        let event_flag = params.redirected_event_flag.unwrap_or(params.event_flag);
        if supports_redirection {
            Self::post_message(
                &vmbus_core::protocol::OpenChannel2 {
                    open_channel,
                    connection_id: params.connection_id,
                    event_flag,
                    flags,
                },
                sink,
            );
        } else {
            Self::post_message(&open_channel, sink);
        }

        let entry = self.channels.get_mut(&channel_id).expect("validated above");
        entry.connection_id = params.connection_id;
        entry.phase = ChannelPhase::Opening {
            request_id,
            redirected_event_flag: params.redirected_event_flag,
        };
        self.outstanding
            .insert(request_id, PendingRequest::OpenChannel { channel_id });
    }

    /// Complete a pending [`Event::OpenChannel`] with an error and
    /// free any caller-supplied event flag.
    fn reject_open<S: ActionSink + ?Sized>(
        &mut self,
        request_id: RequestId,
        redirected_event_flag: Option<u16>,
        error: OpenChannelError,
        sink: &mut S,
    ) {
        if let Some(flag) = redirected_event_flag {
            self.free_event_flag_and_notify(flag, sink);
        }
        sink.emit(Action::Complete {
            request_id,
            result: CompletionResult::OpenChannel(Err(error)),
        });
    }

    /// Handle host-originated `OpenResult` — complete the outstanding
    /// [`Event::OpenChannel`] with success or failure, and emit the
    /// per-channel observables for the wrapper's Arc<AtomicU32>
    /// tracking.
    fn handle_open_result<S: ActionSink + ?Sized>(
        &mut self,
        result: vmbus_core::protocol::OpenResult,
        sink: &mut S,
    ) {
        let channel_id = result.channel_id;
        let Some(entry) = self.channels.get_mut(&channel_id) else {
            return;
        };
        let old_phase = core::mem::replace(&mut entry.phase, ChannelPhase::Offered);
        let ChannelPhase::Opening {
            request_id,
            redirected_event_flag,
        } = old_phase
        else {
            // Stale response — restore and drop.
            entry.phase = old_phase;
            return;
        };
        self.outstanding.remove(&request_id);
        let succeeded = result.status == vmbus_core::protocol::STATUS_SUCCESS as u32;
        if !succeeded {
            let entry = self.channels.get_mut(&channel_id).expect("validated above");
            entry.connection_id = 0;
            if let Some(flag) = redirected_event_flag {
                self.free_event_flag_and_notify(flag, sink);
            }
            sink.emit(Action::ChannelObservable {
                channel_id,
                event: ChannelObservable::ConnectionIdCleared,
            });
            sink.emit(Action::Complete {
                request_id,
                result: CompletionResult::OpenChannel(Err(OpenChannelError::HostFailed(
                    result.status as i32,
                ))),
            });
            return;
        }
        // Success — transition to Opened.
        let entry = self.channels.get_mut(&channel_id).expect("validated above");
        entry.phase = ChannelPhase::Opened {
            redirected_event_flag,
        };
        let connection_id = entry.connection_id;
        sink.emit(Action::ChannelObservable {
            channel_id,
            event: ChannelObservable::ConnectionIdAssigned(connection_id),
        });
        sink.emit(Action::ChannelObservable {
            channel_id,
            event: ChannelObservable::Opened,
        });
        sink.emit(Action::Complete {
            request_id,
            result: CompletionResult::OpenChannel(Ok(OpenChannelSuccess {
                redirected_event_flag,
            })),
        });
    }

    /// Handle [`Event::RestoreChannel`] — validate that the channel
    /// was created in [`ChannelPhase::Restored`], apply the persisted
    /// event flag / connection id, transition to
    /// [`ChannelPhase::Opened`]. Emits its completion synchronously
    /// (no wire message).
    fn handle_restore_channel<S: ActionSink + ?Sized>(
        &mut self,
        request_id: RequestId,
        channel_id: vmbus_core::protocol::ChannelId,
        params: RestoreChannelParams,
        sink: &mut S,
    ) {
        if !self
            .channels
            .get(&channel_id)
            .is_some_and(|entry| matches!(entry.phase, ChannelPhase::Restored))
        {
            self.reject_open(
                request_id,
                params.redirected_event_flag,
                OpenChannelError::InvalidState,
                sink,
            );
            return;
        }
        let entry = self.channels.get_mut(&channel_id).expect("validated above");
        entry.connection_id = params.connection_id;
        entry.phase = ChannelPhase::Opened {
            redirected_event_flag: params.redirected_event_flag,
        };
        sink.emit(Action::ChannelObservable {
            channel_id,
            event: ChannelObservable::ConnectionIdAssigned(params.connection_id),
        });
        sink.emit(Action::ChannelObservable {
            channel_id,
            event: ChannelObservable::Opened,
        });
        sink.emit(Action::Complete {
            request_id,
            result: CompletionResult::OpenChannel(Ok(OpenChannelSuccess {
                redirected_event_flag: params.redirected_event_flag,
            })),
        });
    }

    /// Handle [`Event::CloseChannel`] — fire-and-forget from the
    /// caller. Transitions [`ChannelPhase::Opened`] back to
    /// [`ChannelPhase::Offered`], frees any redirected event flag,
    /// and posts `CloseChannel`. No-op if the channel is already
    /// [`ChannelPhase::Revoked`] (host has already dropped it).
    fn handle_close_channel<S: ActionSink + ?Sized>(
        &mut self,
        channel_id: vmbus_core::protocol::ChannelId,
        sink: &mut S,
    ) {
        let Some(entry) = self.channels.get_mut(&channel_id) else {
            return;
        };
        match entry.phase {
            ChannelPhase::Opened {
                redirected_event_flag,
            } => {
                if let Some(flag) = redirected_event_flag {
                    self.free_event_flag_and_notify(flag, sink);
                }
                let entry = self.channels.get_mut(&channel_id).expect("validated above");
                entry.phase = ChannelPhase::Offered;
                entry.connection_id = 0;
                Self::post_message(&vmbus_core::protocol::CloseChannel { channel_id }, sink);
                sink.emit(Action::ChannelObservable {
                    channel_id,
                    event: ChannelObservable::ConnectionIdCleared,
                });
                sink.emit(Action::ChannelObservable {
                    channel_id,
                    event: ChannelObservable::Closed,
                });
            }
            ChannelPhase::Revoked => {
                // Host already dropped; no wire message needed.
            }
            _ => {
                // Invalid phase — drop silently. The wrapper should
                // gate close on Opened state; a warning here would
                // fire on legitimate revoked-races.
            }
        }
    }

    /// Handle [`Event::ModifyChannel`] — post a `ModifyChannel` wire
    /// message and stash the request id. Duplicates on the same
    /// channel are rejected with a synthetic non-zero status.
    fn handle_modify_channel<S: ActionSink + ?Sized>(
        &mut self,
        request_id: RequestId,
        channel_id: vmbus_core::protocol::ChannelId,
        request: ModifyRequest,
        sink: &mut S,
    ) {
        if !matches!(
            self.phase,
            ClientPhase::Connected { version } if version.version >= Version::Iron
        ) {
            sink.emit(Action::Complete {
                request_id,
                result: CompletionResult::ModifyChannel(-1),
            });
            return;
        }
        let Some(entry) = self.channels.get_mut(&channel_id) else {
            sink.emit(Action::Complete {
                request_id,
                result: CompletionResult::ModifyChannel(-1),
            });
            return;
        };
        if entry.modify_request_id.is_some() {
            sink.emit(Action::Complete {
                request_id,
                result: CompletionResult::ModifyChannel(-1),
            });
            return;
        }
        entry.modify_request_id = Some(request_id);
        self.outstanding
            .insert(request_id, PendingRequest::ModifyChannel { channel_id });
        match request {
            ModifyRequest::TargetVp { target_vp } => {
                Self::post_message(
                    &vmbus_core::protocol::ModifyChannel {
                        channel_id,
                        target_vp,
                    },
                    sink,
                );
            }
        }
    }

    /// Handle host-originated `ModifyChannelResponse` — complete the
    /// outstanding [`Event::ModifyChannel`] for this channel.
    fn handle_modify_channel_response<S: ActionSink + ?Sized>(
        &mut self,
        response: vmbus_core::protocol::ModifyChannelResponse,
        sink: &mut S,
    ) {
        let channel_id = response.channel_id;
        let Some(entry) = self.channels.get_mut(&channel_id) else {
            return;
        };
        let Some(request_id) = entry.modify_request_id.take() else {
            return;
        };
        self.outstanding.remove(&request_id);
        sink.emit(Action::Complete {
            request_id,
            result: CompletionResult::ModifyChannel(response.status),
        });
        self.try_release_channel(channel_id, sink);
    }

    /// Handle [`Event::ReleaseChannel`] — mark the channel
    /// as caller-released. If it's already `Revoked` with no
    /// outstanding requests, [`Self::try_release_channel`] will
    /// post `RelIdReleased` and drop the entry.
    fn handle_release_channel<S: ActionSink + ?Sized>(
        &mut self,
        channel_id: vmbus_core::protocol::ChannelId,
        sink: &mut S,
    ) {
        // If the caller drops a still-Opened channel, close it
        // implicitly (matches vmbus_client's handle_device_removal).
        if let Some(entry) = self.channels.get(&channel_id) {
            if matches!(entry.phase, ChannelPhase::Opened { .. }) {
                self.handle_close_channel(channel_id, sink);
            }
        }
        if let Some(entry) = self.channels.get_mut(&channel_id) {
            entry.is_client_released = true;
        }
        self.try_release_channel(channel_id, sink);
    }

    /// If the channel is caller-released, host-revoked, and has no
    /// outstanding requests, post `RelIdReleased` and drop the entry.
    /// Matches `vmbus_client`'s `Channel::try_release` semantics.
    fn try_release_channel<S: ActionSink + ?Sized>(
        &mut self,
        channel_id: vmbus_core::protocol::ChannelId,
        sink: &mut S,
    ) {
        let Some(entry) = self.channels.get(&channel_id) else {
            return;
        };
        let is_revoked = matches!(entry.phase, ChannelPhase::Revoked);
        let has_pending = entry.modify_request_id.is_some()
            || entry
                .gpadls
                .values()
                .any(|g| !matches!(g, GpadlPhase::Created));
        if entry.is_client_released && is_revoked && !has_pending {
            Self::post_message(&vmbus_core::protocol::RelIdReleased { channel_id }, sink);
            self.channels.remove(&channel_id);
        }
    }

    /// Return the given event flag to the internal pool and notify
    /// the wrapper via [`Action::FreeEventFlag`] so it can drop its
    /// `pal_event::Event` mapping.
    ///
    /// The internal `FlagAllocator::free` asserts on double-free —
    /// callers are expected to have obtained `flag` from a paired
    /// [`Self::allocate_event_flag`] call and never emit
    /// `FreeEventFlag` twice for the same flag.
    fn free_event_flag_and_notify<S: ActionSink + ?Sized>(&mut self, flag: u16, sink: &mut S) {
        self.flag_allocator.free(flag);
        sink.emit(Action::FreeEventFlag(flag));
    }

    /// Handle [`Event::EstablishGpadl`] — validate the channel and
    /// that the gpadl id is fresh, then post `GpadlHeader` with the
    /// GPA values that fit inline, followed by zero or more
    /// `GpadlBody` messages carrying the remainder. Transitions the
    /// per-gpadl state to [`GpadlPhase::Offered`].
    ///
    /// A duplicate gpadl id on the same channel — protocol
    /// violation from the caller side — is completed with an error
    /// and the request is not sent.
    fn handle_establish_gpadl<S: ActionSink + ?Sized>(
        &mut self,
        request_id: RequestId,
        channel_id: vmbus_core::protocol::ChannelId,
        gpadl_id: vmbus_core::protocol::GpadlId,
        request: GpadlRequest,
        sink: &mut S,
    ) {
        let Ok(len_bytes) = size_of_val(request.buf.as_slice()).try_into() else {
            sink.emit(Action::Complete {
                request_id,
                result: CompletionResult::EstablishGpadl(Err(EstablishGpadlError::RequestTooLarge)),
            });
            return;
        };
        let Some(entry) = self.channels.get_mut(&channel_id) else {
            sink.emit(Action::Complete {
                request_id,
                result: CompletionResult::EstablishGpadl(Err(EstablishGpadlError::UnknownChannel)),
            });
            return;
        };
        if entry.gpadls.contains_key(&gpadl_id) {
            sink.emit(Action::Complete {
                request_id,
                result: CompletionResult::EstablishGpadl(Err(EstablishGpadlError::DuplicateId)),
            });
            return;
        }
        entry.gpadls.insert(
            gpadl_id,
            GpadlPhase::Offered {
                request_id,
                teardown_request_ids: alloc::vec::Vec::new(),
            },
        );
        self.outstanding.insert(
            request_id,
            PendingRequest::EstablishGpadl {
                channel_id,
                gpadl_id,
            },
        );

        // Split the buffer: as many u64 values fit inline in
        // GpadlHeader as MAX_DATA_VALUES; the remainder is chunked
        // into GpadlBody messages.
        let buf = request.buf.as_slice();
        let (first, remaining) = if buf.len() > vmbus_core::protocol::GpadlHeader::MAX_DATA_VALUES {
            buf.split_at(vmbus_core::protocol::GpadlHeader::MAX_DATA_VALUES)
        } else {
            (buf, [].as_slice())
        };
        // `len` is the total number of GPA-value bytes, not GpadlHeader
        // fields — matches `vmbus_client::handle_gpadl`.
        let header = vmbus_core::protocol::GpadlHeader {
            channel_id,
            gpadl_id,
            len: len_bytes,
            count: request.count,
        };
        // SAFETY: the wire types are IntoBytes + Immutable; casting
        // &[u64] to &[u8] via zerocopy is the sanctioned pattern.
        Self::post_message_with_data(&header, zerocopy::IntoBytes::as_bytes(first), sink);

        let body = vmbus_core::protocol::GpadlBody { rsvd: 0, gpadl_id };
        for chunk in remaining.chunks(vmbus_core::protocol::GpadlBody::MAX_DATA_VALUES) {
            Self::post_message_with_data(&body, zerocopy::IntoBytes::as_bytes(chunk), sink);
        }
    }

    /// Handle host-originated `GpadlCreated` — complete the pending
    /// [`Event::EstablishGpadl`] with success or failure.
    fn handle_gpadl_created<S: ActionSink + ?Sized>(
        &mut self,
        created: vmbus_core::protocol::GpadlCreated,
        sink: &mut S,
    ) {
        let channel_id = created.channel_id;
        let gpadl_id = created.gpadl_id;
        let Some(entry) = self.channels.get_mut(&channel_id) else {
            return;
        };
        let Some(gpadl_state) = entry.gpadls.get_mut(&gpadl_id) else {
            return;
        };
        // Only Offered can be promoted; a stale GpadlCreated for a
        // gpadl in Created or TearingDown is dropped without state
        // change. Never panic on host input.
        let (request_id, teardown_request_ids) = match gpadl_state {
            GpadlPhase::Offered {
                request_id,
                teardown_request_ids,
            } => (*request_id, core::mem::take(teardown_request_ids)),
            _ => return,
        };
        let succeeded = created.status == vmbus_core::protocol::STATUS_SUCCESS;
        self.outstanding.remove(&request_id);
        if succeeded {
            *gpadl_state = if teardown_request_ids.is_empty() {
                GpadlPhase::Created
            } else {
                GpadlPhase::TearingDown {
                    request_ids: teardown_request_ids,
                }
            };
            sink.emit(Action::Complete {
                request_id,
                result: CompletionResult::EstablishGpadl(Ok(())),
            });
            if matches!(gpadl_state, GpadlPhase::TearingDown { .. })
                && let alloc::collections::btree_map::Entry::Vacant(entry) =
                    self.teardown_gpadls.entry(gpadl_id)
            {
                entry.insert(channel_id);
                Self::post_message(
                    &vmbus_core::protocol::GpadlTeardown {
                        channel_id,
                        gpadl_id,
                    },
                    sink,
                );
            }
        } else {
            entry.gpadls.remove(&gpadl_id);
            sink.emit(Action::Complete {
                request_id,
                result: CompletionResult::EstablishGpadl(Err(EstablishGpadlError::HostRejected(
                    created.status,
                ))),
            });
            for request_id in teardown_request_ids {
                self.outstanding.remove(&request_id);
                sink.emit(Action::Complete {
                    request_id,
                    result: CompletionResult::TeardownGpadl,
                });
            }
        }
        self.try_release_channel(channel_id, sink);
    }

    /// Handle [`Event::TeardownGpadl`] — post `GpadlTeardown` if the
    /// gpadl is in [`GpadlPhase::Created`]; queue on the pending list
    /// if a teardown is already in flight; complete immediately with
    /// no-op on unknown gpadls. If establishment is still in flight,
    /// retain the request until the host accepts or rejects the GPADL.
    fn handle_teardown_gpadl<S: ActionSink + ?Sized>(
        &mut self,
        request_id: RequestId,
        channel_id: vmbus_core::protocol::ChannelId,
        gpadl_id: vmbus_core::protocol::GpadlId,
        sink: &mut S,
    ) {
        let Some(entry) = self.channels.get_mut(&channel_id) else {
            sink.emit(Action::Complete {
                request_id,
                result: CompletionResult::TeardownGpadl,
            });
            return;
        };
        let Some(gpadl_state) = entry.gpadls.get_mut(&gpadl_id) else {
            // Unknown gpadl — treat as already torn down. This
            // matches vmbus_client, which just logs and drops.
            sink.emit(Action::Complete {
                request_id,
                result: CompletionResult::TeardownGpadl,
            });
            return;
        };
        match gpadl_state {
            GpadlPhase::Offered {
                teardown_request_ids,
                ..
            } => {
                teardown_request_ids.push(request_id);
                self.outstanding.insert(
                    request_id,
                    PendingRequest::TeardownGpadl {
                        channel_id,
                        gpadl_id,
                    },
                );
            }
            GpadlPhase::Created => {
                let request_ids = alloc::vec![request_id];
                *gpadl_state = GpadlPhase::TearingDown { request_ids };
                self.outstanding.insert(
                    request_id,
                    PendingRequest::TeardownGpadl {
                        channel_id,
                        gpadl_id,
                    },
                );
                if let alloc::collections::btree_map::Entry::Vacant(entry) =
                    self.teardown_gpadls.entry(gpadl_id)
                {
                    entry.insert(channel_id);
                    Self::post_message(
                        &vmbus_core::protocol::GpadlTeardown {
                            channel_id,
                            gpadl_id,
                        },
                        sink,
                    );
                }
            }
            GpadlPhase::TearingDown { request_ids } => {
                // Coalesce: multiple callers racing to tear down the
                // same gpadl all get completed by the same GpadlTorndown.
                request_ids.push(request_id);
                self.outstanding.insert(
                    request_id,
                    PendingRequest::TeardownGpadl {
                        channel_id,
                        gpadl_id,
                    },
                );
            }
        }
    }

    /// Handle host-originated `GpadlTorndown` — remove the gpadl
    /// entry and complete every teardown request that was coalesced
    /// onto it.
    fn handle_gpadl_torndown<S: ActionSink + ?Sized>(
        &mut self,
        torndown: vmbus_core::protocol::GpadlTorndown,
        sink: &mut S,
    ) {
        let gpadl_id = torndown.gpadl_id;
        let channel_id = match self.teardown_gpadls.get(&gpadl_id).copied() {
            Some(channel_id) => channel_id,
            None => return,
        };
        let Some(entry) = self.channels.get_mut(&channel_id) else {
            return;
        };
        let Some(gpadl) = entry.gpadls.get_mut(&gpadl_id) else {
            return;
        };
        let GpadlPhase::TearingDown { request_ids } = gpadl else {
            return;
        };
        let request_ids = core::mem::take(request_ids);
        entry.gpadls.remove(&gpadl_id);
        self.teardown_gpadls.remove(&gpadl_id);
        for request_id in request_ids {
            self.outstanding.remove(&request_id);
            sink.emit(Action::Complete {
                request_id,
                result: CompletionResult::TeardownGpadl,
            });
        }
        self.start_next_gpadl_teardown(gpadl_id, sink);
        self.try_release_channel(channel_id, sink);
    }

    fn start_next_gpadl_teardown<S: ActionSink + ?Sized>(
        &mut self,
        gpadl_id: vmbus_core::protocol::GpadlId,
        sink: &mut S,
    ) {
        let next_channel_id = self.channels.iter().find_map(|(&channel_id, entry)| {
            let phase = entry.gpadls.get(&gpadl_id)?;
            matches!(phase, GpadlPhase::TearingDown { .. }).then_some(channel_id)
        });
        if let Some(channel_id) = next_channel_id {
            self.teardown_gpadls.insert(gpadl_id, channel_id);
            Self::post_message(
                &vmbus_core::protocol::GpadlTeardown {
                    channel_id,
                    gpadl_id,
                },
                sink,
            );
        }
    }

    /// Handle [`Event::Unload`] — post `Unload` and transition to
    /// [`ClientPhase::Disconnecting`]. If offer enumeration is still
    /// in progress, cancel it before unloading so its caller is not
    /// left pending.
    fn handle_unload<S: ActionSink + ?Sized>(&mut self, request_id: RequestId, sink: &mut S) {
        let version = match self.phase {
            ClientPhase::Connected { version } => version,
            ClientPhase::RequestingOffers {
                version,
                request_id: offers_request_id,
                ..
            } => {
                self.outstanding.remove(&offers_request_id);
                sink.emit(Action::Complete {
                    request_id: offers_request_id,
                    result: CompletionResult::RequestOffers(Err(ConnectError::InvalidState)),
                });
                version
            }
            // Idempotent from Disconnected — nothing to do.
            _ => {
                sink.emit(Action::Complete {
                    request_id,
                    result: CompletionResult::Unload,
                });
                return;
            }
        };
        self.outstanding.insert(request_id, PendingRequest::Unload);
        self.phase = ClientPhase::Disconnecting {
            version,
            request_id,
        };
        Self::post_message(&vmbus_core::protocol::Unload {}, sink);
    }

    /// Handle `UnloadComplete` — finish the caller's Unload and
    /// transition back to [`ClientPhase::Disconnected`].
    fn handle_unload_complete<S: ActionSink + ?Sized>(&mut self, sink: &mut S) {
        let old = core::mem::replace(&mut self.phase, ClientPhase::Disconnected);
        let ClientPhase::Disconnecting { request_id, .. } = old else {
            self.phase = old;
            return;
        };
        self.outstanding.remove(&request_id);
        sink.emit(Action::Complete {
            request_id,
            result: CompletionResult::Unload,
        });
    }

    /// Handle [`Event::ModifyConnection`] — post `ModifyConnection`
    /// with new monitor page GPAs. Rejects if not connected, if the
    /// negotiated feature flags don't include `modify_connection`,
    /// or if another `ModifyConnection` is already in flight.
    fn handle_modify_connection<S: ActionSink + ?Sized>(
        &mut self,
        request_id: RequestId,
        monitor_page: MonitorPageGpas,
        sink: &mut S,
    ) {
        let supported = matches!(
            self.phase,
            ClientPhase::Connected { version } if version.feature_flags.modify_connection()
        );
        if !supported {
            sink.emit(Action::Complete {
                request_id,
                result: CompletionResult::ModifyConnection(
                    vmbus_core::protocol::ConnectionState::FAILED_UNKNOWN_FAILURE,
                ),
            });
            return;
        }
        if self.modify_connection_request_id.is_some() {
            sink.emit(Action::Complete {
                request_id,
                result: CompletionResult::ModifyConnection(
                    vmbus_core::protocol::ConnectionState::FAILED_UNKNOWN_FAILURE,
                ),
            });
            return;
        }
        self.modify_connection_request_id = Some(request_id);
        self.outstanding
            .insert(request_id, PendingRequest::ModifyConnection);
        Self::post_message(
            &vmbus_core::protocol::ModifyConnection {
                parent_to_child_monitor_page_gpa: monitor_page.parent_to_child,
                child_to_parent_monitor_page_gpa: monitor_page.child_to_parent,
            },
            sink,
        );
    }

    /// Handle `ModifyConnectionResponse` — complete the outstanding
    /// [`Event::ModifyConnection`] with the host-supplied connection
    /// state.
    fn handle_modify_connection_response<S: ActionSink + ?Sized>(
        &mut self,
        response: vmbus_core::protocol::ModifyConnectionResponse,
        sink: &mut S,
    ) {
        let Some(request_id) = self.modify_connection_request_id.take() else {
            return;
        };
        self.outstanding.remove(&request_id);
        sink.emit(Action::Complete {
            request_id,
            result: CompletionResult::ModifyConnection(response.connection_state),
        });
    }

    /// Handle [`Event::HvsockConnect`] — post `TlConnectRequest2` and
    /// track the pending caller side by (service_id, endpoint_id).
    /// The host may respond either with a matching `OfferChannel`
    /// (success — handled in [`Self::handle_offer`]) or with
    /// `TlConnectResult` carrying a failure status.
    fn handle_hvsock_connect<S: ActionSink + ?Sized>(
        &mut self,
        request_id: RequestId,
        request: HvsockConnectRequest,
        sink: &mut S,
    ) {
        if !matches!(
            self.phase.version(),
            Some(version) if version.version >= Version::Win10Rs5
        ) {
            sink.emit(Action::Complete {
                request_id,
                result: CompletionResult::HvsockConnect(None),
            });
            return;
        }
        // vmbus_client only sends the newer TlConnectRequest2 (Win10Rs5+).
        let msg = vmbus_core::protocol::TlConnectRequest2 {
            base: vmbus_core::protocol::TlConnectRequest {
                endpoint_id: request.endpoint_id,
                service_id: request.service_id,
            },
            silo_id: request.silo_id,
        };
        self.hvsock_pending
            .entry((request.service_id, request.endpoint_id))
            .or_default()
            .push_back(request_id);
        self.outstanding.insert(
            request_id,
            PendingRequest::HvsockConnect {
                service_id: request.service_id,
            },
        );
        Self::post_message(&msg, sink);
    }

    /// Handle `TlConnectResult` — the host reports a failure (any
    /// success is signalled by an `OfferChannel`, never by
    /// `TlConnectResult`). Completes the pending hvsock request
    /// with `None`.
    fn handle_tl_connect_result<S: ActionSink + ?Sized>(
        &mut self,
        result: vmbus_core::protocol::TlConnectResult,
        sink: &mut S,
    ) {
        // Only failures arrive here; a non-negative status is a
        // protocol violation.
        if result.status >= 0 {
            return;
        }
        let Some(request_id) = self.pop_hvsock_request((result.service_id, result.endpoint_id))
        else {
            return;
        };
        self.outstanding.remove(&request_id);
        sink.emit(Action::Complete {
            request_id,
            result: CompletionResult::HvsockConnect(None),
        });
    }

    fn pop_hvsock_request(&mut self, key: (Guid, Guid)) -> Option<RequestId> {
        let request_ids = self.hvsock_pending.get_mut(&key)?;
        let request_id = request_ids.pop_front();
        if request_ids.is_empty() {
            self.hvsock_pending.remove(&key);
        }
        request_id
    }

    fn handle_prepare_save<S: ActionSink + ?Sized>(&mut self, sink: &mut S) {
        if let Some(request_id) = self.modify_connection_request_id.take() {
            self.outstanding.remove(&request_id);
            sink.emit(Action::Complete {
                request_id,
                result: CompletionResult::ModifyConnection(
                    vmbus_core::protocol::ConnectionState::FAILED_UNKNOWN_FAILURE,
                ),
            });
        }

        for request_ids in core::mem::take(&mut self.hvsock_pending).into_values() {
            for request_id in request_ids {
                self.outstanding.remove(&request_id);
                sink.emit(Action::Complete {
                    request_id,
                    result: CompletionResult::HvsockConnect(None),
                });
            }
        }
    }

    /// Handle [`Event::Pause`] — post `Pause` wire message. Only
    /// meaningful when the negotiated feature set includes
    /// `pause_resume`; on older versions the message is a no-op on
    /// the host side and will be silently discarded.
    fn handle_pause<S: ActionSink + ?Sized>(&mut self, sink: &mut S) {
        Self::post_message(&vmbus_core::protocol::Pause, sink);
    }

    /// Handle [`Event::Resume`] — post `Resume`.
    fn handle_resume<S: ActionSink + ?Sized>(&mut self, sink: &mut S) {
        Self::post_message(&vmbus_core::protocol::Resume, sink);
    }

    /// Handle `PauseResponse` — signal acknowledged. Currently no
    /// per-request tracking; the wrapper uses this as a hint to
    /// stop draining its incoming-message queue until the next
    /// [`Event::Resume`].
    fn handle_pause_response<S: ActionSink + ?Sized>(&mut self, sink: &mut S) {
        sink.emit(Action::PauseComplete);
    }
}

#[cfg(test)]
mod step_tests;

/// Integration tests that drive a `ClientCore` through a realistic
/// sequence of caller [`Event`]s and host [`Event::HostMessage`]
/// payloads, checking the emitted [`Action`]s and phase transitions
/// against an ordered script. These are the "golden path" tests any
/// wrapper adopting `vmbus_client_core` (either the
/// `vmbus_client` `ClientTask` rewrite or a `vmbus_guest`
/// state-machine driver) is expected to satisfy.
#[cfg(test)]
mod integration_tests;
