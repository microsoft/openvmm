// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Thin wrappers around the two hypercalls used by the vmbus protocol:
//! `HvCallPostMessage` (0x5C) and `HvCallSignalEvent` (0x5D).
//!
//! Callers construct a [`HvTestCtx`](opentmk::platform::hyperv::ctx::HvTestCtx)
//! and pass it here through the [`HypercallTrait`] abstraction so ownership
//! of the hypercall input/output page stays inside the ctx (see §5 of
//! `tasks/vmbus-port-design.md`).

use crate::Error;
use crate::Result;
use hvdef::HypercallCode;
use hvdef::hypercall::PostMessage;
use hvdef::hypercall::SignalEvent;
use opentmk::context::HypercallConfig;
use opentmk::context::HypercallTrait;
use zerocopy::IntoBytes;

/// Post a message to the specified VMBus connection.
///
/// * `connection_id` — 1 for versions < 5.0, 4 for versions ≥ 5.0 unless
///   the host returned a different id in `VersionResponse`.
/// * `payload` — the encoded VMBus message including its `MessageHeader`.
///
/// The payload is truncated at `HV_MESSAGE_PAYLOAD_SIZE` (240 bytes).
pub fn post_message<C: HypercallTrait>(
    ctx: &mut C,
    connection_id: u32,
    payload: &[u8],
) -> Result<()> {
    if payload.len() > hvdef::HV_MESSAGE_PAYLOAD_SIZE {
        return Err(Error::Parse {
            ty: None,
            reason: "post_message payload exceeds HV_MESSAGE_PAYLOAD_SIZE",
        });
    }

    let mut msg = PostMessage {
        connection_id,
        padding: 0,
        message_type: crate::protocol::HV_MESSAGE_TYPE_CHANNEL,
        payload_size: payload.len() as u32,
        payload: [0; 240],
    };
    msg.payload[..payload.len()].copy_from_slice(payload);

    ctx.hypercall(
        HypercallCode::HvCallPostMessage.0 as u64,
        msg.as_bytes(),
        &mut [],
        HypercallConfig::default(),
    )?;
    Ok(())
}

/// Signal an event flag on the specified connection.
///
/// Used on the ring-buffer send path to notify the host that data has
/// been produced (§4 of the design doc — the "signal path" bullets).
pub fn signal_event<C: HypercallTrait>(
    ctx: &mut C,
    connection_id: u32,
    flag_number: u16,
) -> Result<()> {
    let msg = SignalEvent {
        connection_id,
        flag_number,
        rsvd: 0,
    };
    ctx.hypercall(
        HypercallCode::HvCallSignalEvent.0 as u64,
        msg.as_bytes(),
        &mut [],
        HypercallConfig {
            pass_by_register_hint: true,
            ..HypercallConfig::default()
        },
    )?;
    Ok(())
}
