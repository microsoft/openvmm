// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-side VMBus client for opentmk-invariant.
//!
//! This crate implements the guest half of the VMBus protocol. It targets
//! `#![no_std] + alloc` and runs inside a UEFI image alongside the rest of
//! the opentmk framework.
//!
//! # Scope
//!
//! * SynIC bring-up on the current VP (SIMP/SIEFP/SCONTROL/SINT2).
//! * `InitiateContact`/`VersionResponse` negotiation with newest-to-oldest
//!   fallback down to `Version::Copper` (6.0).
//! * `RequestOffers`/`OfferChannel`/`AllOffersDelivered` enumeration.
//! * `GpadlHeader`/`GpadlBody`/`GpadlTeardown` create/teardown.
//! * `OpenChannel`/`OpenChannelResult`/`CloseChannel`/`RelIdReleased`.
//! * Ring-buffer send/recv with correct empty→non-empty signalling.
//! * Message-page draining with EOM under SINT2.
//! * `Unload`/`UnloadComplete` on shutdown.
//! * hvsocket wire structs (`TlConnectRequest[/2]`, `TlConnectResult`,
//!   `HvsockUserDefinedParameters`) only — no high-level socket API.
//!
//! # Design entry point
//!
//! The public entry point is [`init`]. Callers construct and initialise a
//! [`HvTestCtx`](opentmk::platform::hyperv::ctx::HvTestCtx), then pass it
//! into [`init`] so all hypercalls flow through
//! [`HypercallTrait`].
//!
//! # Status
//!
//! Fully implemented and unit-tested on the host target:
//!
//! * All wire types (round-trip via `zerocopy`).
//! * Ring-buffer send/recv with wraparound and signal semantics.
//! * GPADL header/body encoder + completion flow.
//! * Message completion table with per-request keys.
//! * Version-negotiation state machine.
//! * `RequestOffers` enumeration.
//! * `OpenChannel[/2]` / `CloseChannel` / `RelIdReleased`.
//! * `TlConnectRequest[/2]` encoder + result callback.
//! * SIMP-slot draining + EOM.
//!
//! The following are UEFI-target only (host tests exercise the
//! state-machine layer with a mock ctx and scripted pump):
//!
//! * SynIC page allocation (`init_synic`).
//! * `SimpPump` reading from a real SIMP GPA.
//!
//! Follow-up work explicitly out of scope for the initial port:
//!
//! * Real SINT2 ISR installation (we use polling with a bounded retry
//!   inside [`SimpPump`](interrupt::SimpPump)).
//! * Ring buffer allocation helpers (caller supplies the ring GPADL).
//! * Rescind teardown semantics on [`channel::Channel`] beyond acking.
//! * Reserved channels, `ModifyChannel`, monitor-page signalling,
//!   confidential VMBus.
//! * A high-level hv-socket / pipe API (only the wire helpers ship).

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod channel;
pub mod connection;
pub mod devices;
mod error;
pub mod gpadl;
pub mod hvsock;
pub mod hypercalls;
pub mod interrupt;
pub mod message;
pub mod protocol;
pub mod ring;
pub mod synic;

#[cfg(test)]
mod tests;

pub use error::Error;
pub use error::Result;

use alloc::vec::Vec;
use opentmk::context::HypercallTrait;

/// Initialise the guest-side VMBus stack: bring up SynIC on the current VP,
/// negotiate a protocol version with the host, and prime the message
/// dispatcher.
///
/// After this call succeeds, use [`request_offers`] to enumerate channels
/// and [`open_channel`](channel::open_channel) to open one.
///
/// Delegates to [`synic::init_synic`] and [`connection::initiate`].
pub fn init<C: HypercallTrait>(ctx: &mut C) -> Result<()> {
    synic::init_synic(ctx)?;
    connection::initiate(ctx)?;
    Ok(())
}

/// Post a `RequestOffers` and collect the returned `OfferChannel` messages
/// until `AllOffersDelivered` is received.
pub fn request_offers<C: HypercallTrait>(ctx: &mut C) -> Result<Vec<protocol::OfferChannel>> {
    connection::request_offers(ctx)
}

/// Post an `Unload` and wait for `UnloadComplete`, then clear the
/// process-wide connection state.
pub fn unload<C: HypercallTrait>(ctx: &mut C) -> Result<()> {
    connection::unload(ctx)
}
