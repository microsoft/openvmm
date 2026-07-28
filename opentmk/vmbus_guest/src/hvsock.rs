// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! hv-socket wire types and a `TlConnectRequest[2]` helper.
//!
//! §7 of `tasks/vmbus-port-design.md` deliberately keeps this to a
//! structs-only pass plus a single `send_hvsock_connect` helper — we
//! don't yet implement listen/accept, pipe framing, or a stream API.
//!
//! # Follow-up work
//!
//! * Listen side: register a service GUID with the host so
//!   incoming `TlConnectRequest` messages route to a callback.
//! * Pipe framing: `PipeType::BYTE` / `PipeType::MESSAGE` on top of the
//!   ring layer (see `vmbus_ring::pipe_protocol` in openvmm).
//! * A real socket / stream API layered on top of the pipe framing.

use crate::Error;
use crate::Result;
use crate::protocol::Guid;
use crate::protocol::TlConnectRequest;
use crate::protocol::TlConnectRequest2;
use crate::protocol::TlConnectResult;
use opentmk::context::HypercallTrait;

pub use crate::protocol::HvsockParametersVersion;
pub use crate::protocol::HvsockUserDefinedParameters;

/// Post a `TlConnectRequest` (or `TlConnectRequest2` when a silo id is
/// supplied and the negotiated version is ≥ RS5).
///
/// The completion (`TlConnectResult`) is delivered asynchronously via
/// the message dispatcher; register a callback with
/// [`set_connect_result_handler`] before calling this.
///
/// **Not implemented in the scaffold.**
pub fn send_hvsock_connect<C: HypercallTrait>(
    _ctx: &mut C,
    _endpoint: Guid,
    _service: Guid,
    _silo: Option<Guid>,
) -> Result<()> {
    // TODO(vmbus-port): pick between `TlConnectRequest` and
    // `TlConnectRequest2` based on the negotiated version + whether
    // `silo.is_some()`, encode, and post via `hypercalls::post_message`.
    Err(Error::NotImplemented)
}

/// Callback fired when the host sends `TlConnectResult`.
pub type ConnectResultHandler = fn(&TlConnectResult);

/// Register a handler to receive `TlConnectResult` messages.
///
/// **Not implemented in the scaffold.**
pub fn set_connect_result_handler(_handler: ConnectResultHandler) {
    // TODO(vmbus-port): store the handler in a `Mutex<Option<_>>` and
    // invoke it from the message dispatcher when a `TL_CONNECT_RESULT`
    // arrives.
}

// Silence dead-code warnings for the unused imports the scaffold leaves
// in place so callers see the intended shape.
// Reference the wire types so IDE navigation/consumers see the shape.
fn _reference_types(_a: TlConnectRequest, _b: TlConnectRequest2, _c: HvsockUserDefinedParameters) {}
