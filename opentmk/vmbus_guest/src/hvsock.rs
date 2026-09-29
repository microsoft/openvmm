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
use crate::client_driver::driver;
use alloc::vec::Vec;
use core::mem::size_of;
use guid::Guid;
use opentmk_core::context::HypercallPlatformTrait;
use opentmk_core::platform::hyperv::ctx::HyperVHypercallConfig;
use spin::Mutex;
use vmbus_core::protocol::HEADER_SIZE;
use vmbus_core::protocol::MessageHeader;
use vmbus_core::protocol::MessageType;
use vmbus_core::protocol::TlConnectRequest;
use vmbus_core::protocol::TlConnectRequest2;
use vmbus_core::protocol::TlConnectResult;
use vmbus_core::protocol::Version;
use zerocopy::IntoBytes;

pub use vmbus_core::protocol::HvsockParametersVersion;
pub use vmbus_core::protocol::HvsockUserDefinedParameters;

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
/// any. Called by the synchronous client driver before it routes the
/// host result into `ClientCore`.
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
pub fn send_hvsock_connect<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
    ctx: &mut C,
    endpoint: Guid,
    service: Guid,
    silo: Option<Guid>,
) -> Result<()> {
    let mut driver = driver();
    if driver.version().is_none() {
        return Err(Error::VersionMismatch);
    }
    let request_id = driver.request_id();
    driver.step(
        ctx,
        vmbus_client_core::Event::HvsockConnect {
            request_id,
            request: vmbus_client_core::HvsockConnectRequest {
                service_id: service,
                endpoint_id: endpoint,
                silo_id: silo.unwrap_or_default(),
                hosted_silo_unaware: silo.is_none(),
            },
        },
    )?;
    driver.detach_request(request_id);
    Ok(())
}
