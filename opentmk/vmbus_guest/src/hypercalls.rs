// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Thin wrappers around the hypercalls used by the vmbus protocol:
//! `HvCallPostMessage` (0x5C), `HvCallSignalEvent` (0x5D), and
//! `HvCallSetVpRegisters` (0x51).
//!
//! Callers construct a [`HvTestCtx`](opentmk_core::platform::hyperv::ctx::HvTestCtx)
//! and pass it here through the [`HypercallPlatformTrait`] abstraction so
//! ownership of the hypercall input/output page stays inside the
//! ctx — the ctx owns the aligned pages and the calling convention;
//! this crate contributes only the payload encoders.

use crate::Error;
use crate::Result;
use crate::protocol::HV_MESSAGE_TYPE_CHANNEL;
use alloc::vec;
use alloc::vec::Vec;
use core::hint::spin_loop;
use core::mem::size_of;
use hvdef::HV_PARTITION_ID_SELF;
use hvdef::HV_VP_INDEX_SELF;
use hvdef::HvRegisterName;
use hvdef::HvRegisterValue;
use hvdef::HypercallCode;
use hvdef::hypercall::GetSetVpRegisters;
use hvdef::hypercall::HvInputVtl;
use hvdef::hypercall::HvRegisterAssoc;
use hvdef::hypercall::PostMessage;
use hvdef::hypercall::SignalEvent;
use opentmk_core::context::HypercallPlatformTrait;
use opentmk_core::platform::hyperv::ctx::HyperVHypercallConfig;
use opentmk_core::tmkdefs::TmkError;
use zerocopy::IntoBytes;

/// Post a message to the specified VMBus connection.
///
/// * `connection_id` — 1 for versions < 5.0, 4 for versions ≥ 5.0 unless
///   the host returned a different id in `VersionResponse`.
/// * `payload` — the encoded VMBus message including its `MessageHeader`.
///
/// The payload is truncated at `HV_MESSAGE_PAYLOAD_SIZE` (240 bytes).
///
/// Retries transient `HV_STATUS_INSUFFICIENT_BUFFERS` up to
/// [`POST_MESSAGE_MAX_RETRIES`] times. The hypervisor's per-VP
/// message queue can transiently reject posts under load — bursts of
/// GPADL body messages (netvsp establishes a 16 MiB recv buffer via
/// ~147 back-to-back posts) will otherwise trip it. Matches Linux's
/// `vmbus_post_msg` (drivers/hv/connection.c) which uses the same
/// bounded retry approach.
pub fn post_message<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
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
        message_type: HV_MESSAGE_TYPE_CHANNEL,
        payload_size: payload.len() as u32,
        payload: [0; 240],
    };
    msg.payload[..payload.len()].copy_from_slice(payload);

    log::debug!(
        "post_message: conn_id={} payload_len={}",
        connection_id,
        payload.len()
    );
    for attempt in 0..POST_MESSAGE_MAX_RETRIES {
        let r = ctx.hypercall(
            HypercallCode::HvCallPostMessage.0 as u64,
            msg.as_bytes(),
            &mut [],
            HyperVHypercallConfig::default(),
        );
        match r {
            Ok(()) => {
                if attempt > 0 {
                    log::debug!("post_message: succeeded after {} retries", attempt);
                }
                return Ok(());
            }
            Err(e @ (TmkError::InsufficientBuffers | TmkError::InsufficientMemory)) => {
                // Transient — the per-VP message queue is full or the
                // hypervisor is under memory pressure. Both are the
                // same class of resource-exhaustion status Linux's
                // `vmbus_post_msg` treats as -EAGAIN.
                if attempt + 1 == POST_MESSAGE_MAX_RETRIES {
                    log::warn!(
                        "post_message: gave up after {} transient retries (last={:?})",
                        POST_MESSAGE_MAX_RETRIES,
                        e,
                    );
                }
                // Exponential backoff, capped, to give the hypervisor
                // room to drain under sustained pressure. Linux's
                // vmbus_post_msg does up to MAX_UDELAY_MS per attempt.
                let shift = attempt.min(POST_MESSAGE_BACKOFF_MAX_SHIFT);
                let iters = POST_MESSAGE_BACKOFF_ITERS.saturating_mul(1usize << shift);
                for _ in 0..iters {
                    spin_loop();
                }
                continue;
            }
            Err(e) => {
                log::debug!("post_message: hypercall returned Err({:?})", e);
                return Err(Error::Hypercall(e));
            }
        }
    }
    Err(Error::Hypercall(TmkError::InsufficientBuffers))
}

/// Number of attempts (including the first) before giving up on a
/// transient `post_message` failure. Matches Linux's
/// `MAX_MSG_RETRY_COUNT = 100`.
pub const POST_MESSAGE_MAX_RETRIES: usize = 100;

/// Base spin-loop iterations between `post_message` retries. The
/// backoff doubles up to [`POST_MESSAGE_BACKOFF_MAX_SHIFT`] and then
/// caps out. Not a wall-clock duration, but ~10 000 spin_loops is a
/// few microseconds on modern CPUs.
pub const POST_MESSAGE_BACKOFF_ITERS: usize = 10_000;

/// Cap on the exponential-backoff shift. `1 << 12 = 4096`, i.e. the
/// max per-iteration wait is ~40M spin_loops (single-digit ms) —
/// comparable to Linux's `MAX_UDELAY_MS`.
pub const POST_MESSAGE_BACKOFF_MAX_SHIFT: usize = 12;

/// Signal an event flag on the specified connection.
///
/// Used on the ring-buffer send path to notify the host that data
/// has been produced. Guest→host signals always use `flag_number = 0`
/// per Hyper-V convention (matches `vmbus_client::guest_to_host_interrupt`
/// in openvmm); the per-channel `event_flag` field is for the
/// **host→guest** direction only (bit position in the SIEFP page).
pub fn signal_event<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
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
        HyperVHypercallConfig {
            fast_call: true,
            ..HyperVHypercallConfig::default()
        },
    )?;
    Ok(())
}

/// Set a single VP register on the current VP via
/// [`HvCallSetVpRegisters`](HypercallCode::HvCallSetVpRegisters).
///
/// Convenience wrapper used by [`crate::synic::init_synic`] to program
/// SIMP / SIEFP / SCONTROL / SINT2. The rep-hypercall is issued with
/// `rep_count = 1`.
pub fn set_vp_register<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
    ctx: &mut C,
    name: HvRegisterName,
    value: HvRegisterValue,
) -> Result<()> {
    set_vp_registers(ctx, HvInputVtl::CURRENT_VTL, &[(name, value)])
}

/// Set N VP registers on the current VP in a single hypercall.
///
/// `assocs` is a slice of `(name, value)` pairs; the hypercall is issued
/// with `rep_count = assocs.len()`.
pub fn set_vp_registers<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
    ctx: &mut C,
    target_vtl: HvInputVtl,
    assocs: &[(HvRegisterName, HvRegisterValue)],
) -> Result<()> {
    if assocs.is_empty() {
        return Ok(());
    }

    let header = GetSetVpRegisters {
        partition_id: HV_PARTITION_ID_SELF,
        vp_index: HV_VP_INDEX_SELF,
        target_vtl,
        rsvd: [0; 3],
    };

    let mut input = Vec::with_capacity(
        size_of::<GetSetVpRegisters>() + assocs.len() * size_of::<HvRegisterAssoc>(),
    );
    input.extend_from_slice(header.as_bytes());
    for &(name, value) in assocs {
        let assoc = HvRegisterAssoc {
            name,
            pad: [0; 3],
            value,
        };
        input.extend_from_slice(assoc.as_bytes());
    }

    ctx.hypercall(
        HypercallCode::HvCallSetVpRegisters.0 as u64,
        &input,
        &mut [],
        HyperVHypercallConfig {
            rep_count: Some(assocs.len()),
            ..HyperVHypercallConfig::default()
        },
    )?;
    Ok(())
}

/// Read N VP registers on the current VP in a single hypercall.
///
/// Returns their values in the same order as the input `names`.
/// Only the low 64 bits are returned per register — enough for the
/// SynIC registers we use to confirm state.
pub fn get_vp_registers<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
    ctx: &mut C,
    target_vtl: HvInputVtl,
    names: &[HvRegisterName],
) -> Result<Vec<u64>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }

    let header = GetSetVpRegisters {
        partition_id: HV_PARTITION_ID_SELF,
        vp_index: HV_VP_INDEX_SELF,
        target_vtl,
        rsvd: [0; 3],
    };

    let mut input = Vec::with_capacity(size_of::<GetSetVpRegisters>() + size_of_val(names));
    input.extend_from_slice(header.as_bytes());
    for &n in names {
        input.extend_from_slice(n.as_bytes());
    }

    // Output layout: one `HvRegisterValue` (16 bytes) per register.
    let mut output = vec![0u8; names.len() * size_of::<HvRegisterValue>()];

    ctx.hypercall(
        HypercallCode::HvCallGetVpRegisters.0 as u64,
        &input,
        &mut output,
        HyperVHypercallConfig {
            rep_count: Some(names.len()),
            ..HyperVHypercallConfig::default()
        },
    )?;

    let mut values = Vec::with_capacity(names.len());
    for i in 0..names.len() {
        let base = i * size_of::<HvRegisterValue>();
        let low = u64::from_le_bytes(output[base..base + 8].try_into().unwrap());
        values.push(low);
    }
    Ok(values)
}
