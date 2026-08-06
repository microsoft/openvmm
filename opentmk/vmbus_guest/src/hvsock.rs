// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! hv-socket wire helpers.
//!
//! Initial pass covers:
//! * the wire structs (re-exported from [`crate::protocol`]),
//! * an outbound `TlConnectRequest[/2]` helper, and
//! * a callback slot for the resulting `TlConnectResult`.
//!
//! # Follow-up work (out of scope for this port)
//!
//! * Listen side: register a service GUID with the host so incoming
//!   `TlConnectRequest`s route to a callback.
//! * Pipe framing (`PipeType::BYTE` / `PipeType::MESSAGE` on top of
//!   the ring layer — see `vmbus_ring::pipe_protocol` in openvmm).
//! * A real socket/stream API layered on top of the pipe framing.

use crate::Error;
use crate::Result;
use crate::protocol::Guid;
use crate::protocol::HEADER_SIZE;
use crate::protocol::MessageHeader;
use crate::protocol::MessageType;
use crate::protocol::TlConnectRequest;
use crate::protocol::TlConnectRequest2;
use crate::protocol::TlConnectResult;
use crate::protocol::Version;
use alloc::vec::Vec;
use core::mem::size_of;
use opentmk::context::HypercallTrait;
use spin::Mutex;
use zerocopy::IntoBytes;

pub use crate::protocol::HvsockParametersVersion;
pub use crate::protocol::HvsockUserDefinedParameters;

/// Callback fired when the host sends `TlConnectResult`.
pub type ConnectResultHandler = fn(&TlConnectResult);

/// Global handler slot. `None` when no client has registered.
static HANDLER: Mutex<Option<ConnectResultHandler>> = Mutex::new(None);

/// Register a handler to receive `TlConnectResult` messages.
///
/// The handler is installed globally and replaces any previous
/// registration. To unregister, pass a no-op handler (there is no
/// explicit `unregister` API).
///
/// The handler is invoked from the message-page drain (see
/// [`crate::interrupt`]), so it runs in whatever context the pump
/// runs in.
pub fn set_connect_result_handler(handler: ConnectResultHandler) {
    *HANDLER.lock() = Some(handler);
}

/// Dispatch a decoded `TlConnectResult` to the registered handler, if
/// any. Called from [`crate::message::route_message`] via the
/// [`MessageSink::tl_connect_result`](crate::message::MessageSink)
/// hook when a completion arrives.
pub fn dispatch_connect_result(result: &TlConnectResult) {
    if let Some(handler) = *HANDLER.lock() {
        handler(result);
    }
}

/// Encode a `TlConnectRequest[/2]` for the given endpoint / service /
/// silo, picking the wire layout based on the negotiated version and
/// whether a silo id was supplied.
///
/// Returns the byte buffer ready to be posted via
/// [`crate::hypercalls::post_message`].
pub fn encode_tl_connect_request(
    version: Version,
    endpoint_id: Guid,
    service_id: Guid,
    silo: Option<Guid>,
) -> Vec<u8> {
    let use_v2 = silo.is_some() && version >= Version::Win10Rs5;
    let mut buf = Vec::new();
    if use_v2 {
        let msg = TlConnectRequest2 {
            base: TlConnectRequest {
                endpoint_id,
                service_id,
            },
            silo_id: silo.unwrap_or_default(),
        };
        buf.reserve(HEADER_SIZE + size_of::<TlConnectRequest2>());
        buf.extend_from_slice(MessageHeader::new(MessageType::TL_CONNECT_REQUEST).as_bytes());
        buf.extend_from_slice(msg.as_bytes());
    } else {
        let msg = TlConnectRequest {
            endpoint_id,
            service_id,
        };
        buf.reserve(HEADER_SIZE + size_of::<TlConnectRequest>());
        buf.extend_from_slice(MessageHeader::new(MessageType::TL_CONNECT_REQUEST).as_bytes());
        buf.extend_from_slice(msg.as_bytes());
    }
    buf
}

/// Post a `TlConnectRequest[/2]` and return immediately without
/// waiting for `TlConnectResult`. The result arrives asynchronously
/// through the message-page drain and is delivered to the handler
/// registered via [`set_connect_result_handler`].
///
/// Requires a negotiated connection ([`crate::connection::initiate`])
/// to have completed.
pub fn send_hvsock_connect<C: HypercallTrait>(
    ctx: &mut C,
    endpoint: Guid,
    service: Guid,
    silo: Option<Guid>,
) -> Result<()> {
    let state = crate::connection::connection()
        .clone()
        .ok_or(Error::VersionMismatch)?;
    let payload = encode_tl_connect_request(state.selected_version, endpoint, service, silo);
    crate::hypercalls::post_message(ctx, state.post_message_connection_id, &payload)
}
