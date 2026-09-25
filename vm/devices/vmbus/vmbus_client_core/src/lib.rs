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
