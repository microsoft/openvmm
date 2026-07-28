// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `InitiateContact` / `VersionResponse` negotiation and top-level
//! connection state.
//!
//! Implements the version-negotiation ladder from §3 of the design doc
//! (Copper → Iron → Rs5 → Rs4 → Rs3_1 → Win10 → Win8_1 → Win8) and
//! `RequestOffers` / `AllOffersDelivered` enumeration.

use crate::Error;
use crate::Result;
use crate::protocol::FeatureFlags;
use crate::protocol::OfferChannel;
use crate::protocol::Version;
use alloc::vec::Vec;
use opentmk::context::HypercallTrait;
use spin::Mutex;

/// Top-level connection state established after successful negotiation.
pub struct ConnectionState {
    /// Version selected by the host.
    pub selected_version: Version,
    /// Connection ID to use for `HvCallPostMessage` from now on.
    pub post_message_connection_id: u32,
    /// Feature flags supported by both sides.
    pub feature_flags: FeatureFlags,
    /// Server-provided parent → child monitor page GPA, if any.
    pub parent_to_child_monitor_page_gpa: u64,
    /// Server-provided child → parent monitor page GPA, if any.
    pub child_to_parent_monitor_page_gpa: u64,
}

/// The single shared connection state.
static CONNECTION: Mutex<Option<ConnectionState>> = Mutex::new(None);

/// Return a reference to the negotiated connection state, if any.
pub fn connection() -> spin::MutexGuard<'static, Option<ConnectionState>> {
    CONNECTION.lock()
}

/// Attempt `InitiateContact` for each version in
/// [`Version::NEGOTIATION_LADDER`] until one succeeds, then record the
/// result in [`connection`].
///
/// Returns [`Error::VersionMismatch`] if no version is accepted.
///
/// **Not implemented in the scaffold** — the follow-up patch wires up
/// the outbound `InitiateContact[2]` encoding, the message-page reply
/// wait loop, and the `VersionResponse` / `VersionResponse2` /
/// `VersionResponse3` variant selection.
pub fn initiate<C: HypercallTrait>(_ctx: &mut C) -> Result<()> {
    // TODO(vmbus-port): implement the negotiation walk per §3.
    Err(Error::NotImplemented)
}

/// Post `RequestOffers` and collect the returned `OfferChannel` payloads
/// until `AllOffersDelivered` is received.
///
/// **Not implemented in the scaffold.**
pub fn request_offers<C: HypercallTrait>(_ctx: &mut C) -> Result<Vec<OfferChannel>> {
    Err(Error::NotImplemented)
}

/// Send `Unload` and wait for `UnloadComplete`.
///
/// **Not implemented in the scaffold.**
pub fn unload<C: HypercallTrait>(_ctx: &mut C) -> Result<()> {
    Err(Error::NotImplemented)
}
