// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-side fuzz handlers for the VMBus protocol itself.
//!
//! Where [`crate::functions::netvsp`] fuzzes a *device* sitting on top
//! of a well-behaved VMBus client, these handlers fuzz the VMBus layer
//! underneath it: arbitrary bytes posted as channel messages, and
//! arbitrary packets (including self-inconsistent descriptors) written
//! into a channel's ring.
//!
//! They back the host `vmbus_fuzzer` grammar and are a port of the
//! legacy puppet `vmbus_fuzzer`'s syscall surface:
//!
//! * `vmbus_msg`       — post a raw channel message, fire-and-forget.
//! * `vmbus_msg_comp`  — post a raw channel message and copy the
//!   host's reply back into a fuzzer-visible output buffer.
//! * `vmbus_packet`    — write a packet into the ring of an opened
//!   channel, optionally with a fully fuzzer-controlled descriptor.
//! * `vmbus_reopen_channel` — close and reopen that channel.
//! * `vmbus_fill_relids` — hand the fuzzer real `child_relid` values so
//!   its generated messages address channels the host knows about.
//!
//! The VMBus connection (SynIC + negotiation + offers) is brought up
//! lazily on first use and kept warm, since a testcase that never gets
//! past `InitiateContact` reaches almost none of the host's code.

// UNSAFETY: this module holds the vmbus session in a `static` behind a
// mutex, which requires an `unsafe impl Send` for the wrapper type.
#![expect(unsafe_code)]

use crate::functions::{FuzzFunctionVariable, VerifyFuzzVariables};
#[cfg_attr(not(target_os = "uefi"), expect(unused_imports))]
use crate::prelude::*;
use hvdef::Vtl;
use inv_decoder::SafeMemoryMap;
use opentmk_core::platform::hyperv::ctx::HvTestCtx;
use spin::Mutex;
use vmbus_guest::fuzz::{self, PACKET_DESCRIPTOR_SIZE, RawChannel};
use vmbus_guest::protocol::{OfferChannel, PacketFlags, PacketType};

/// Upper bound on a fuzzer-supplied message length before we stage it.
///
/// The length is untrusted input, so an unbounded `vec![0u8; len]`
/// would abort on a capacity overflow long before the host saw
/// anything. A channel message can't exceed the 240-byte hypervisor
/// payload anyway; the extra slack lets over-long lengths through to
/// the truncation path in [`fuzz::post_raw_message`] so that
/// "declared longer than it is" stays reachable.
const MAX_FUZZ_MSG_LEN: usize = 4096;

/// Upper bound on a fuzzer-supplied ring packet payload. The ring has
/// 32 KiB of data per direction; anything near that is rejected before
/// it can wedge the ring for the rest of the testcase.
const MAX_FUZZ_PKT_LEN: usize = 16 * 1024;

/// Number of relids `vmbus_fill_relids` writes, matching the grammar's
/// `array[RelId, 10]`.
const FILL_RELIDS_COUNT: usize = 10;

/// Bounded drain applied at each testcase boundary — both the number
/// of stale host messages discarded and the number of inbound ring
/// packets consumed.
const RESET_DRAIN_LIMIT: usize = 64;

/// A live VMBus connection plus the channel used for ring traffic.
struct VmbusSession {
    /// Hypercall context. Boxed because `HvTestCtx` embeds two inline
    /// 4 KiB hypercall pages, which overflow the bare-metal UEFI stack
    /// as a by-value local (see the netvsp handler for the same note).
    ctx: Box<HvTestCtx>,
    /// Offers as delivered by the host, used by `vmbus_fill_relids`
    /// and to pick the channel to open.
    offers: Vec<OfferChannel>,
    /// Channel backing `vmbus_packet`. Opened lazily: a run that only
    /// ever posts channel messages never needs a ring.
    chan: Option<RawChannel>,
}

/// Newtype so the session can live in a `static` behind a [`Mutex`].
/// [`RawChannel`] holds raw pointers to its ring region, so it isn't
/// automatically [`Send`].
struct SessionCell {
    session: Option<VmbusSession>,
}

// SAFETY: the guest fuzz executor is single-threaded; the raw pointers
// held by `RawChannel`/`HvTestCtx` are only accessed while holding the
// `VMBUS` mutex, from that single thread. Mirrors the equivalent
// `unsafe impl Send` on the netvsp session.
unsafe impl Send for SessionCell {}

static VMBUS: Mutex<SessionCell> = Mutex::new(SessionCell { session: None });

/// Round-robin cursor over the offer list for `vmbus_fill_relids`, so
/// successive calls hand out different channels rather than always the
/// first. Ports puppet's `LAST_CHAN_IND`.
static RELID_CURSOR: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Bring up SynIC + the VMBus connection and enumerate offers.
fn bring_up_session() -> Result<VmbusSession, String> {
    let mut ctx = Box::new(HvTestCtx::new());
    ctx.init(Vtl::Vtl0)
        .map_err(|e| format!("vmbus: HvTestCtx init failed: {e}"))?;

    vmbus_guest::init(&mut *ctx).map_err(|e| format!("vmbus: init failed: {e:?}"))?;
    let offers = vmbus_guest::request_offers(&mut *ctx)
        .map_err(|e| format!("vmbus: offers failed: {e:?}"))?;
    log::info!("vmbus: brought up connection with {} offers", offers.len());

    Ok(VmbusSession {
        ctx,
        offers,
        chan: None,
    })
}

/// Choose the offer to open for ring traffic.
///
/// Prefers netvsp: it is the device most likely to be present, has the
/// richest host-side packet parsing, and matches what the legacy
/// puppet fuzzer used. Falls back to the first offer so the handler
/// still works on a VM without a synthetic NIC.
fn preferred_offer(offers: &[OfferChannel]) -> Option<OfferChannel> {
    offers
        .iter()
        .find(|o| o.interface_id == vmbus_guest::devices::netvsp::INTERFACE_GUID)
        .or_else(|| offers.first())
        .copied()
}

/// Run `f` against the lazily-initialized session.
fn with_session<R>(f: impl FnOnce(&mut VmbusSession) -> Result<R, String>) -> Result<R, String> {
    let mut cell = VMBUS.lock();
    if cell.session.is_none() {
        cell.session = Some(bring_up_session()?);
    }
    let session = cell
        .session
        .as_mut()
        .expect("session initialized immediately above");
    f(session)
}

/// Run `f` against the session's ring channel, opening it on first use.
fn with_channel<R>(
    f: impl FnOnce(&mut RawChannel, &mut HvTestCtx) -> Result<R, String>,
) -> Result<R, String> {
    with_session(|s| {
        if s.chan.is_none() {
            let offer = preferred_offer(&s.offers)
                .ok_or_else(|| String::from("vmbus: no offers to open a channel on"))?;
            log::debug!(
                "vmbus: opening raw channel on offer iface={:x?} chan_id={:?} conn_id={:#x}",
                offer.interface_id,
                offer.channel_id,
                offer.connection_id
            );
            let chan = RawChannel::open(&mut *s.ctx, &offer)
                .map_err(|e| format!("vmbus: channel open failed: {e:?}"))?;
            log::info!(
                "vmbus: opened raw channel {:?}",
                chan.channel().channel_id()
            );
            s.chan = Some(chan);
        }
        let VmbusSession { ctx, chan, .. } = s;
        let chan = chan.as_mut().expect("channel opened immediately above");
        f(chan, ctx)
    })
}

/// Run a handler body, logging any runtime error rather than surfacing
/// it.
///
/// A runtime failure (malformed input, a guest-memory fault, a host
/// that never answers) must not abort the campaign, so it is logged
/// and the handler reports success. Parameter *decoding* is still
/// checked with `?` by the caller, since a bad parameter count means a
/// grammar/handler mismatch worth failing on.
fn run_logged(f: impl FnOnce() -> Result<(), String>) -> Result<FuzzFunctionVariable, String> {
    if let Err(e) = f() {
        log::error!("{e}");
    }
    Ok(FuzzFunctionVariable::Void)
}

/// Read a bounded, fuzzer-supplied byte range out of guest memory.
fn read_input(
    mem: &mut dyn SafeMemoryMap,
    what: &str,
    ptr: usize,
    len: usize,
    max: usize,
) -> Result<Vec<u8>, String> {
    if len > max {
        return Err(format!("{what}: length {len} exceeds max {max}"));
    }
    let mut buf = vec![0u8; len];
    if len > 0 {
        mem.try_read_mem(ptr, &mut buf)
            .map_err(|e| format!("{what}: failed to read input: {e}"))?;
    }
    Ok(buf)
}

/// Reset VMBus state at a testcase boundary.
///
/// The fuzzer posts arbitrary channel messages on the *live* negotiated
/// connection, so a testcase can drive host-side connection or channel
/// state that the guest's cached session never mirrors (see the module
/// docs). Left in place, that host<->guest desync makes a later
/// testcase's legitimate channel-open wedge, and the host resets the
/// whole VM.
///
/// To keep each testcase hermetic we cycle the connection every
/// boundary, reusing the SynIC + hypercall context so the reset costs a
/// handshake rather than a full SynIC re-program:
///
/// 1. Drain and discard any host messages the previous testcase
///    provoked but never read, so they can't be misread during the
///    teardown handshake or by the next testcase.
/// 2. Close the ring channel if one was opened, freeing its ring and
///    releasing the GPADL host-side.
/// 3. `Unload` the connection so the host returns to a disconnected
///    state and the guest's `CONNECTION` global is cleared.
/// 4. Re-`InitiateContact` and re-`RequestOffers` on the same context,
///    refreshing the offer list for the next testcase.
///
/// Every step is best-effort: a testcase that already desynced the host
/// can make `Unload` or `InitiateContact` time out, in which case we
/// drop the whole session so the next call rebuilds it from scratch (a
/// fresh SynIC included).
pub fn reset_session() {
    let mut cell = VMBUS.lock();
    let Some(session) = cell.session.as_mut() else {
        return;
    };

    // 1. Clear stale host messages from the SIMP page.
    log::debug!("vmbus reset: step 1 drain_pending");
    if let Err(e) = fuzz::drain_pending(&mut *session.ctx, RESET_DRAIN_LIMIT) {
        log::debug!("vmbus: reset drain_pending failed: {e:?}");
    }

    // 2. Close the ring channel, freeing the ring and its GPADL.
    if let Some(chan) = session.chan.take() {
        log::debug!("vmbus reset: step 2 close channel");
        if let Err(e) = chan.close(&mut *session.ctx) {
            log::debug!("vmbus: reset close_channel failed: {e:?}");
        }
    }

    // 3. Tear the connection down so host and guest agree on a clean
    //    slate. `unload` clears the process-wide `CONNECTION` state.
    if vmbus_guest::connection::connection().is_some() {
        log::debug!("vmbus reset: step 3 unload (awaiting UnloadComplete)");
        if let Err(e) = vmbus_guest::unload(&mut *session.ctx) {
            log::debug!("vmbus: reset unload failed: {e:?}");
        }
    }

    // 4. Bring the connection back up on the same SynIC + context.
    log::debug!("vmbus reset: step 4 initiate (awaiting VersionResponse)");
    let reconnect = vmbus_guest::connection::initiate(&mut *session.ctx).and_then(|()| {
        log::debug!("vmbus reset: step 4 request_offers (awaiting AllOffersDelivered)");
        vmbus_guest::request_offers(&mut *session.ctx)
    });
    match reconnect {
        Ok(offers) => {
            log::debug!("vmbus: reset reconnected with {} offers", offers.len());
            session.offers = offers;
        }
        Err(e) => {
            log::info!("vmbus: reset reconnect failed ({e:?}); dropping session for full rebuild");
            cell.session = None;
        }
    }
}

/// `vmbus_msg(pkt, pkt_len, conn_id)` — post a raw channel message.
///
/// * `pkt`      — pointer to the message bytes (a `MessageHeader`
///   followed by a body, as far as the host is concerned).
/// * `pkt_len`  — byte length of `*pkt`.
/// * `conn_id`  — connection id to post on; `0` means the negotiated
///   one. See [`fuzz::resolve_connection_id`].
pub fn vmbus_msg(
    mem: &mut dyn SafeMemoryMap,
    vars: Vec<FuzzFunctionVariable>,
) -> Result<FuzzFunctionVariable, String> {
    let [pkt, pkt_len, conn_id] = vars.verify_num_params()?;
    let pkt = pkt.expect_int("pkt")? as usize;
    let pkt_len = pkt_len.expect_int("pkt_len")? as usize;
    let conn_id = conn_id.expect_int("conn_id")?;

    run_logged(|| {
        log::debug!("vmbus_msg: enter pkt={pkt:#x} len={pkt_len} conn_id={conn_id:#x}");
        let msg = read_input(mem, "vmbus_msg", pkt, pkt_len, MAX_FUZZ_MSG_LEN)?;
        let connection_id = fuzz::resolve_connection_id(conn_id);
        with_session(|s| {
            fuzz::post_raw_message(&mut *s.ctx, connection_id, &msg)
                .map_err(|e| format!("vmbus_msg: {e:?}"))
        })
    })
}

/// `vmbus_msg_comp(pkt, pkt_len, pkt_out, pkt_out_len, conn_id)` —
/// post a raw channel message and capture the host's reply.
///
/// The reply bytes are copied back into `pkt_out` (truncated to
/// `pkt_out_len`), which makes them visible to the fuzzer as an output
/// argument and lets the grammar chain a response field into a later
/// call. Ports puppet's `VmbusChannelMessageComp`.
///
/// A host that sends no reply within the bounded wait is normal, not
/// an error: `pkt_out` is simply left untouched.
pub fn vmbus_msg_comp(
    mem: &mut dyn SafeMemoryMap,
    vars: Vec<FuzzFunctionVariable>,
) -> Result<FuzzFunctionVariable, String> {
    let [pkt, pkt_len, pkt_out, pkt_out_len, conn_id] = vars.verify_num_params()?;
    let pkt = pkt.expect_int("pkt")? as usize;
    let pkt_len = pkt_len.expect_int("pkt_len")? as usize;
    let pkt_out = pkt_out.expect_int("pkt_out")? as usize;
    let pkt_out_len = pkt_out_len.expect_int("pkt_out_len")? as usize;
    let conn_id = conn_id.expect_int("conn_id")?;

    run_logged(|| {
        log::debug!(
            "vmbus_msg_comp: enter pkt={pkt:#x} len={pkt_len} out={pkt_out:#x} out_len={pkt_out_len} conn_id={conn_id:#x}"
        );
        let msg = read_input(mem, "vmbus_msg_comp", pkt, pkt_len, MAX_FUZZ_MSG_LEN)?;
        let connection_id = fuzz::resolve_connection_id(conn_id);
        let resp = with_session(|s| {
            fuzz::post_raw_message_wait(&mut *s.ctx, connection_id, &msg, fuzz::DEFAULT_WAIT_POLLS)
                .map_err(|e| format!("vmbus_msg_comp: {e:?}"))
        })?;

        let Some(resp) = resp else {
            log::debug!("vmbus_msg_comp: no response from host");
            return Ok(());
        };
        if pkt_out == 0 || pkt_out_len == 0 {
            return Ok(());
        }
        let n = resp.len().min(pkt_out_len);
        mem.try_write_mem(pkt_out, &resp[..n])
            .map_err(|e| format!("vmbus_msg_comp: failed to write response: {e}"))
    })
}

/// `vmbus_packet(pkt, pkt_len, pkt_desc, pkt_type)` — write a packet
/// into the channel's send ring.
///
/// * `pkt` / `pkt_len` — the packet payload.
/// * `pkt_desc` — pointer to a 16-byte
///   [`vmbus_guest::protocol::PacketDescriptor`] to emit **verbatim**,
///   or `0` to have the ring build a self-consistent one.
/// * `pkt_type` — the packet type. When `pkt_desc` is supplied, this
///   overwrites the descriptor's type field, so the grammar's typed
///   packet-body variants still line up with the type the host sees
///   (this is what puppet did).
///
/// The verbatim path is the interesting one: `length8` and
/// `data_offset8` can then disagree with the bytes actually in the
/// ring, which is precisely the host parser surface under test.
pub fn vmbus_packet(
    mem: &mut dyn SafeMemoryMap,
    vars: Vec<FuzzFunctionVariable>,
) -> Result<FuzzFunctionVariable, String> {
    let [pkt, pkt_len, pkt_desc, pkt_type] = vars.verify_num_params()?;
    let pkt = pkt.expect_int("pkt")? as usize;
    let pkt_len = pkt_len.expect_int("pkt_len")? as usize;
    let pkt_desc = pkt_desc.expect_int("pkt_desc")? as usize;
    let pkt_type = pkt_type.expect_int("pkt_type")? as u16;

    run_logged(|| {
        log::debug!(
            "vmbus_packet: enter pkt={pkt:#x} len={pkt_len} desc={pkt_desc:#x} type={pkt_type}"
        );
        let payload = read_input(mem, "vmbus_packet", pkt, pkt_len, MAX_FUZZ_PKT_LEN)?;

        let descriptor = if pkt_desc != 0 {
            let mut desc = [0u8; PACKET_DESCRIPTOR_SIZE];
            mem.try_read_mem(pkt_desc, &mut desc)
                .map_err(|e| format!("vmbus_packet: failed to read descriptor: {e}"))?;
            // Force the descriptor's type to the call's `pkt_type` so
            // the body the grammar generated matches the type the host
            // will dispatch on. Field 0 of `PacketDescriptor`.
            desc[..2].copy_from_slice(&pkt_type.to_le_bytes());
            Some(desc)
        } else {
            None
        };

        with_channel(|chan, ctx| match &descriptor {
            Some(desc) => {
                let r = chan
                    .send_raw_packet(ctx, desc, &payload)
                    .map_err(|e| format!("vmbus_packet(raw): {e:?}"));
                log::debug!("vmbus_packet: raw send returned ok={}", r.is_ok());
                r
            }
            None => chan
                .send_packet(ctx, PacketType(pkt_type), &payload, PacketFlags::new())
                .map_err(|e| format!("vmbus_packet: {e:?}")),
        })
    })
}

/// `vmbus_reopen_channel()` — close and reopen the ring channel.
///
/// Ports puppet's `ReopenChannelMessage`. Exercises the host's
/// open/close path repeatedly and gives the fuzzer a way to recover a
/// ring that an earlier packet wedged.
pub fn vmbus_reopen_channel(
    _mem: &mut dyn SafeMemoryMap,
    vars: Vec<FuzzFunctionVariable>,
) -> Result<FuzzFunctionVariable, String> {
    let [] = vars.verify_num_params()?;

    run_logged(|| {
        log::debug!("vmbus_reopen_channel: enter");
        with_session(|s| {
            if let Some(chan) = s.chan.take() {
                if let Err(e) = chan.close(&mut *s.ctx) {
                    log::warn!("vmbus_reopen_channel: close failed: {e:?}");
                }
            }
            let offer = preferred_offer(&s.offers)
                .ok_or_else(|| String::from("vmbus_reopen_channel: no offers"))?;
            let chan = RawChannel::open(&mut *s.ctx, &offer)
                .map_err(|e| format!("vmbus_reopen_channel: open failed: {e:?}"))?;
            s.chan = Some(chan);
            Ok(())
        })
    })
}

/// `vmbus_fill_relids(relids)` — write [`FILL_RELIDS_COUNT`] real
/// `child_relid` values into the fuzzer's buffer.
///
/// Without this the fuzzer would have to guess relids, and essentially
/// every generated channel message would be rejected by the host's
/// first validity check. Successive calls walk the offer list so a
/// program can reach more than one channel.
pub fn vmbus_fill_relids(
    mem: &mut dyn SafeMemoryMap,
    vars: Vec<FuzzFunctionVariable>,
) -> Result<FuzzFunctionVariable, String> {
    let [relids] = vars.verify_num_params()?;
    let relids_ptr = relids.expect_int("relids")? as usize;

    run_logged(|| {
        log::debug!("vmbus_fill_relids: enter relids={relids_ptr:#x}");
        if relids_ptr == 0 {
            return Err(String::from("vmbus_fill_relids: null relids pointer"));
        }
        let ids = with_session(|s| Ok(fuzz::relids(&s.offers)))?;
        if ids.is_empty() {
            return Err(String::from(
                "vmbus_fill_relids: no offers have been delivered",
            ));
        }

        let mut buf = [0u8; FILL_RELIDS_COUNT * 4];
        for chunk in buf.chunks_exact_mut(4) {
            let cursor =
                RELID_CURSOR.fetch_add(1, core::sync::atomic::Ordering::Relaxed) % ids.len();
            chunk.copy_from_slice(&ids[cursor].to_le_bytes());
        }
        mem.try_write_mem(relids_ptr, &buf)
            .map_err(|e| format!("vmbus_fill_relids: failed to write relids: {e}"))
    })
}
