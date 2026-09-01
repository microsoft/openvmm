// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-side fuzz handlers for the Hyper-V synthetic NIC (netvsp).
//!
//! These handlers back the host `netvsp_fuzzer` grammar. On first use
//! they lazily bring up a full netvsp data path (SynIC + VMBus +
//! channel open + version negotiation + NDIS/RNDIS init + packet
//! filter), then keep the session alive for the remainder of the
//! guest's lifetime so subsequent calls operate on a warm channel.
//!
//! The registered calls mirror the legacy puppet netvsp fuzzer:
//!
//! * `send_nvsp`   — post a raw, caller-supplied NVSP message inband.
//! * `send_rndis`  — wrap a raw, caller-supplied RNDIS message in an
//!   NVSP `V1_SEND_RNDIS_PKT` delivered via GPA-direct.
//! * `open_channel` — ensure the netvsp session is established
//!   (idempotent; brings the channel up on first call).
//! * `renew_buffer` — revoke and re-establish the receive or send
//!   buffer, reusing the existing GPADL registration.

// UNSAFETY: this module holds the netvsp session in a `static` behind a
// mutex, which requires an `unsafe impl Send` for the wrapper type.
#![expect(unsafe_code)]

use crate::functions::{FuzzFunctionVariable, VerifyFuzzVariables};
#[cfg_attr(not(target_os = "uefi"), expect(unused_imports))]
use crate::prelude::*;
use hvdef::Vtl;
use inv_decoder::SafeMemoryMap;
use opentmk_core::platform::hyperv::ctx::HvTestCtx;
use spin::Mutex;
use vmbus_guest::devices::netvsp::{self, Netvsp, RMC_CONTROL, RMC_DATA, rndis};

/// Bit 0 of a `send_nvsp` / `send_rndis` flags argument: request a
/// completion for the posted packet (guest spins for the paired
/// `VM_PKT_COMP`). When clear, the send is fire-and-forget.
const FLAG_REQUEST_COMPLETION: u64 = 0x1;

/// Upper bound on a fuzzer-supplied message length before we allocate
/// a staging buffer for it. The length arrives as untrusted fuzz input,
/// so an unbounded `vec![0u8; len]` would panic with a capacity
/// overflow (or exhaust the heap) on a garbage value. Matches the
/// `MAX_RNDIS_LEN` cap enforced deeper in the guest netvsp driver.
const MAX_FUZZ_MSG_LEN: usize = 16 * 4096;

/// Receive-buffer size established during session bring-up.
///
/// The conventional netvsc recv buffer is 16 MiB, but this fuzzer
/// barely exercises the RX path (the host is not flooding us), and the
/// buffer's GPADL is re-registered on **every** per-testcase channel
/// reset — a 16 MiB buffer is 4096 PFNs, which `establish_gpadl` must
/// stream to the host as ~140 ~240-byte `GPADL_BODY` post-messages,
/// dominating reset cost. 256 KiB (64 PFNs → ~3 messages) is ample for
/// the handful of inbound packets we see and collapses that burst ~40×
/// with no loss of host-side coverage (the recv-buffer handling code
/// runs regardless of size).
const RECV_BUFFER_SIZE: usize = 256 * 1024;
/// Send-buffer size established during session bring-up (1 MiB).
const SEND_BUFFER_SIZE: usize = 1024 * 1024;
/// MTU advertised in `SEND_NDIS_CONFIG` (standard Ethernet + VLAN).
const DEFAULT_MTU: u32 = 1514;

/// A live netvsp data path.
///
/// Split into a *persistent* part — the hypercall context (`ctx`) and
/// the netvsp channel `offer` — that survives across testcases, and a
/// *per-testcase* part (`nic`) that is closed and reopened by
/// [`reset_session`] at every testcase boundary. Keeping the VMBus
/// connection up across resets avoids re-running SynIC init and
/// `request_offers` (and leaking their allocations) on every testcase;
/// only the netvsp channel itself is churned.
struct NetvspSession {
    ctx: Box<HvTestCtx>,
    offer: vmbus_guest::protocol::OfferChannel,
    nic: Netvsp,
}

/// Newtype wrapper so we can hold the session in a `static` behind a
/// [`Mutex`]. `Netvsp` transitively contains raw pointers (ring
/// backing, pending-TX allocations) so it is not automatically
/// [`Send`].
struct SessionCell {
    session: Option<NetvspSession>,
}

// SAFETY: the guest fuzz executor is single-threaded; the raw pointers
// held by `Netvsp`/`HvTestCtx` are only ever accessed while holding the
// `NETVSP` mutex, from that single thread. This mirrors the existing
// `unsafe impl Send for OwnedBuf` rationale in the netvsp driver.
unsafe impl Send for SessionCell {}

static NETVSP: Mutex<SessionCell> = Mutex::new(SessionCell { session: None });

/// Bring up the VMBus connection and locate the netvsp channel offer.
///
/// This is the *persistent* half of bring-up (SynIC + VMBus
/// negotiation + channel enumeration); it is done once and reused
/// across per-testcase channel resets.
fn bring_up_vmbus() -> Result<(Box<HvTestCtx>, vmbus_guest::protocol::OfferChannel), String> {
    // `HvTestCtx` embeds two inline 4 KiB hypercall pages (~8 KiB). Keep it
    // boxed so the context never lands on the guest stack: as a by-value
    // local, its frame overflowed the bare-metal (non-growable) UEFI stack
    // during bring-up and faulted the guest before its first instruction.
    let mut ctx = Box::new(HvTestCtx::new());
    ctx.init(Vtl::Vtl0)
        .map_err(|e| format!("netvsp: HvTestCtx init failed: {e}"))?;

    vmbus_guest::init(&mut *ctx).map_err(|e| format!("netvsp: vmbus init failed: {e:?}"))?;
    let offers = vmbus_guest::request_offers(&mut *ctx)
        .map_err(|e| format!("netvsp: request_offers failed: {e:?}"))?;
    let offer = *offers
        .iter()
        .find(|o| o.interface_id == netvsp::INTERFACE_GUID)
        .ok_or_else(|| String::from("netvsp: no netvsp offer found"))?;
    Ok((ctx, offer))
}

/// Open the netvsp channel on an already-established VMBus connection
/// and run the full netvsp/RNDIS bring-up (open + version negotiation +
/// NDIS config/version + recv/send buffers + RNDIS init + packet
/// filter).
///
/// This is the *per-testcase* half of bring-up: called once during the
/// initial [`bring_up_session`] and again after every channel reset in
/// [`reset_session`]. It allocates a fresh ring + recv/send buffers,
/// so the previous channel's [`NetvspBacking`] must be freed first.
fn open_netvsp_channel(
    ctx: &mut HvTestCtx,
    offer: &vmbus_guest::protocol::OfferChannel,
) -> Result<Netvsp, String> {
    let mut nic = Netvsp::open(ctx, offer).map_err(|e| format!("netvsp: open failed: {e:?}"))?;
    nic.negotiate_version(ctx)
        .map_err(|e| format!("netvsp: negotiate_version failed: {e:?}"))?;
    nic.send_ndis_config(ctx, DEFAULT_MTU)
        .map_err(|e| format!("netvsp: send_ndis_config failed: {e:?}"))?;
    nic.send_ndis_version(ctx)
        .map_err(|e| format!("netvsp: send_ndis_version failed: {e:?}"))?;
    nic.establish_recv_buffer(ctx, RECV_BUFFER_SIZE)
        .map_err(|e| format!("netvsp: establish_recv_buffer failed: {e:?}"))?;
    nic.establish_send_buffer(ctx, SEND_BUFFER_SIZE)
        .map_err(|e| format!("netvsp: establish_send_buffer failed: {e:?}"))?;
    nic.rndis_init(ctx)
        .map_err(|e| format!("netvsp: rndis_init failed: {e:?}"))?;
    let filter = rndis::NDIS_PACKET_TYPE_DIRECTED
        | rndis::NDIS_PACKET_TYPE_BROADCAST
        | rndis::NDIS_PACKET_TYPE_ALL_MULTICAST;
    nic.set_packet_filter(ctx, filter)
        .map_err(|e| format!("netvsp: set_packet_filter failed: {e:?}"))?;
    Ok(nic)
}

/// Bring up a complete netvsp data path from scratch: VMBus connection
/// plus an opened netvsp channel.
fn bring_up_session() -> Result<NetvspSession, String> {
    let (mut ctx, offer) = bring_up_vmbus()?;
    let nic = open_netvsp_channel(&mut *ctx, &offer)?;
    Ok(NetvspSession { ctx, offer, nic })
}

/// Run `f` against the (lazily initialized) netvsp session.
///
/// The datapath is brought up on first use and then kept warm. Each
/// testcase boundary calls [`reset_session`] to close and reopen the
/// channel, so a datapath wedged by one testcase (e.g. a `renew_buffer`
/// that revoked the send buffer) cannot leak into the next.
fn with_session<R>(f: impl FnOnce(&mut NetvspSession) -> Result<R, String>) -> Result<R, String> {
    let mut cell = NETVSP.lock();
    if cell.session.is_none() {
        cell.session = Some(bring_up_session()?);
    }
    let session = cell
        .session
        .as_mut()
        .expect("session initialized immediately above");
    f(session)
}

/// Reset the netvsp channel at a testcase boundary.
///
/// Closes the current netvsp channel, **reclaims** its guest memory
/// (ring region + recv/send GPADL buffers + any pending-TX staging
/// buffers), and reopens a fresh channel on the same VMBus connection.
/// This gives each testcase an isolated, clean-slate data path: no
/// wedged send ring, revoked/renewed buffer, or mutated RNDIS packet
/// filter can leak from one testcase into the next. It is also the
/// recovery path for a datapath that a prior testcase wedged (e.g. a
/// `renew_buffer` that revoked the send buffer but failed to
/// re-establish it, after which the host stops completing every send).
///
/// Only the channel is churned — the VMBus connection (SynIC + offers)
/// is kept up, so a reset costs one channel bring-up (~8 round-trips),
/// not a full SynIC/`request_offers` re-init. Freeing the old backing
/// before reopening keeps this leak-free across an unbounded campaign;
/// `close_channel` uses SynIC post-messages (not the data ring) so it
/// succeeds even when the ring is wedged full.
///
/// If reopening fails, the VMBus connection is torn down (`unload`) and
/// the session cleared, so the next [`with_session`] rebuilds from
/// scratch rather than operating on a half-open channel.
pub fn reset_session() {
    let mut cell = NETVSP.lock();
    let Some(session) = cell.session.take() else {
        // No session yet (or a prior reset already cleared it): the
        // next `with_session` call brings one up fresh.
        return;
    };
    let NetvspSession {
        mut ctx,
        offer,
        nic,
    } = session;

    // Close the channel first, then free its backing — the host must
    // stop touching the ring/buffers before we deallocate them.
    let (channel, backing) = nic.into_parts();
    if let Err(e) = vmbus_guest::channel::close_channel(&mut *ctx, channel) {
        log::warn!("netvsp: reset close_channel failed: {e:?}");
    }
    backing.free();

    // Reopen a fresh channel on the same VMBus connection.
    match open_netvsp_channel(&mut *ctx, &offer) {
        Ok(nic) => {
            cell.session = Some(NetvspSession { ctx, offer, nic });
        }
        Err(e) => {
            log::warn!(
                "netvsp: channel reopen after reset failed ({e}); \
                 tearing down VMBus for a full rebuild on next call"
            );
            if let Err(e) = vmbus_guest::unload(&mut *ctx) {
                log::warn!("netvsp: vmbus unload after failed reopen: {e:?}");
            }
            // Leave `cell.session` as None → next `with_session` does a
            // full bring-up (VMBus + channel) from scratch.
        }
    }
}

/// Run a fuzzer-handler body, logging any runtime error instead of
/// surfacing it as a handler error.
///
/// A runtime failure inside a handler (a malformed fuzzing input, a
/// guest-memory read fault, or a netvsp device-side timeout) must not
/// abort the campaign, so the handler logs it and reports success.
/// Parameter decoding is still validated with `?` by the caller *before*
/// this is reached, since a bad parameter count/type indicates a
/// fuzzer/grammar bug worth failing on.
fn run_logged(f: impl FnOnce() -> Result<(), String>) -> Result<FuzzFunctionVariable, String> {
    if let Err(e) = f() {
        log::error!("{e}");
    }
    Ok(FuzzFunctionVariable::Void)
}

/// `send_nvsp(pkt, pkt_len, flags)` — post a raw NVSP message inband.
///
/// * `pkt`     — pointer to the NVSP message bytes.
/// * `pkt_len` — byte length of `*pkt`.
/// * `flags`   — bit 0 requests a completion.
pub fn send_nvsp(
    mem: &mut dyn SafeMemoryMap,
    vars: Vec<FuzzFunctionVariable>,
) -> Result<FuzzFunctionVariable, String> {
    let [pkt, pkt_len, flags] = vars.verify_num_params()?;
    let pkt = pkt.expect_int("pkt")? as usize;
    let pkt_len = pkt_len.expect_int("pkt_len")? as usize;
    let flags = flags.expect_int("flags")?;
    let completion = (flags & FLAG_REQUEST_COMPLETION) != 0;

    run_logged(|| {
        if pkt_len > MAX_FUZZ_MSG_LEN {
            return Err(format!(
                "send_nvsp: pkt_len {pkt_len} exceeds max {MAX_FUZZ_MSG_LEN}"
            ));
        }
        let mut frame = vec![0u8; pkt_len];
        if pkt_len > 0 {
            mem.try_read_mem(pkt, &mut frame)
                .map_err(|e| format!("send_nvsp: failed to read input: {e}"))?;
        }

        with_session(|s| {
            s.nic
                .send_nvsp_raw(&mut *s.ctx, &frame, completion)
                .map_err(|e| format!("send_nvsp: {e:?}"))
        })
    })
}

/// `send_rndis(rndis, rndis_len, flags)` — wrap a raw RNDIS message in
/// an NVSP `V1_SEND_RNDIS_PKT` and post it GPA-direct.
///
/// The channel type ([`RMC_DATA`] vs [`RMC_CONTROL`]) is derived from
/// the RNDIS message type in the first 4 bytes, matching the legacy
/// puppet semantics.
///
/// * `rndis`     — pointer to the RNDIS message bytes.
/// * `rndis_len` — byte length of `*rndis`.
/// * `flags`     — bit 0 requests a completion.
pub fn send_rndis(
    mem: &mut dyn SafeMemoryMap,
    vars: Vec<FuzzFunctionVariable>,
) -> Result<FuzzFunctionVariable, String> {
    let [rndis_ptr, rndis_len, flags] = vars.verify_num_params()?;
    let rndis_ptr = rndis_ptr.expect_int("rndis")? as usize;
    let rndis_len = rndis_len.expect_int("rndis_len")? as usize;
    let flags = flags.expect_int("flags")?;
    let completion = (flags & FLAG_REQUEST_COMPLETION) != 0;

    run_logged(|| {
        if rndis_len == 0 {
            return Err(String::from("send_rndis: empty RNDIS message"));
        }
        if rndis_len > MAX_FUZZ_MSG_LEN {
            return Err(format!(
                "send_rndis: rndis_len {rndis_len} exceeds max {MAX_FUZZ_MSG_LEN}"
            ));
        }
        let mut msg = vec![0u8; rndis_len];
        mem.try_read_mem(rndis_ptr, &mut msg)
            .map_err(|e| format!("send_rndis: failed to read input: {e}"))?;

        // Derive channel type from the RNDIS message type (first u32 LE):
        // data packets ride RMC_DATA, everything else RMC_CONTROL. Short
        // (<4 byte) messages have no parseable type — default to control.
        let channel_type = if msg.len() >= 4
            && u32::from_le_bytes([msg[0], msg[1], msg[2], msg[3]])
                == rndis::MESSAGE_TYPE_PACKET_MSG
        {
            RMC_DATA
        } else {
            RMC_CONTROL
        };

        with_session(|s| {
            s.nic
                .send_rndis_raw(&mut *s.ctx, channel_type, &msg, completion)
                .map_err(|e| format!("send_rndis: {e:?}"))
        })
    })
}

/// `open_channel()` — ensure the netvsp session is established.
///
/// Idempotent: brings the channel up on the first call and is a no-op
/// once a session exists.
pub fn open_channel(
    _mem: &mut dyn SafeMemoryMap,
    vars: Vec<FuzzFunctionVariable>,
) -> Result<FuzzFunctionVariable, String> {
    let [] = vars.verify_num_params()?;
    run_logged(|| with_session(|_| Ok(())))
}

/// `renew_buffer(is_send_buffer)` — revoke and re-establish a buffer.
///
/// * `is_send_buffer` — 0 renews the receive buffer, non-zero renews
///   the send buffer. Both reuse the existing GPADL registration (no
///   new allocation).
pub fn renew_buffer(
    _mem: &mut dyn SafeMemoryMap,
    vars: Vec<FuzzFunctionVariable>,
) -> Result<FuzzFunctionVariable, String> {
    let [is_send_buffer] = vars.verify_num_params()?;
    let is_send_buffer = is_send_buffer.expect_int("is_send_buffer")? != 0;

    run_logged(|| {
        with_session(|s| {
            if is_send_buffer {
                s.nic
                    .renew_send_buffer(&mut *s.ctx)
                    .map_err(|e| format!("renew_buffer(send): {e:?}"))
            } else {
                s.nic
                    .renew_recv_buffer(&mut *s.ctx)
                    .map_err(|e| format!("renew_buffer(recv): {e:?}"))
            }
        })
    })
}
