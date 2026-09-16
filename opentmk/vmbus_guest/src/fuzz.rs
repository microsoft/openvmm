// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Raw VMBus primitives for fuzzing the host-side vmbus implementation.
//!
//! Everything else in this crate is a *well-behaved* VMBus client: it
//! encodes typed messages, keys completions off ids it allocated, and
//! keeps its own ring bookkeeping consistent with what it tells the
//! host. That is the wrong shape for a fuzzer, which needs to hand the
//! host arbitrary bytes on both the message and the ring path and then
//! observe what comes back.
//!
//! This module is the escape hatch. It is the guest half of the
//! `vmbus_fuzzer` grammar and a port of the legacy puppet
//! `vmbus_fuzzer`'s five syscall shapes:
//!
//! | puppet `SyscallType`      | here                                  |
//! |---------------------------|---------------------------------------|
//! | `VmbusChannelMessage`     | [`post_raw_message`]                  |
//! | `VmbusChannelMessageComp` | [`post_raw_message_wait`]             |
//! | `VmbusPacketMessage`      | [`RawChannel::send_raw_packet`] / [`RawChannel::send_packet`] |
//! | `ReopenChannelMessage`    | [`RawChannel::close`] + [`RawChannel::open`] |
//! | `FillRelIds`              | [`relids`]                            |
//!
//! # Safety posture
//!
//! The fuzzer drives this code with untrusted input, so every entry
//! point here is bounded and total:
//!
//! * Message payloads are truncated to [`MAX_MESSAGE_SIZE`] rather
//!   than rejected, so a fuzzed length can't fail the call before the
//!   host ever sees the bytes.
//! * Every wait is bounded by an explicit poll count and returns
//!   `Ok(None)` on expiry — a fuzzed request that the host chooses not
//!   to answer stalls one testcase, never the campaign.
//! * A host reply that doesn't parse is logged and returned as raw
//!   bytes (see [`crate::interrupt::drain_once_capture`]) instead of
//!   propagating an error out of a half-consumed SIMP slot.

use crate::Error;
use crate::Result;
use crate::channel::Channel;
use crate::channel::close_channel;
use crate::channel::open_channel;
use crate::connection::connection;
use crate::gpadl::establish_gpadl;
use crate::gpadl::teardown_gpadl;
use crate::hypercalls::post_message;
#[cfg_attr(
    not(target_os = "uefi"),
    expect(unused_imports, reason = "used only by the UEFI SIMP drain")
)]
use crate::message::completion_table;
use crate::protocol::MAX_MESSAGE_SIZE;
use crate::protocol::OfferChannel;
use crate::protocol::PacketFlags;
use crate::protocol::PacketType;
use crate::protocol::VMBUS_CONNECTION_ID_LEGACY;
use crate::protocol::VMBUS_CONNECTION_ID_MODERN;
use crate::protocol::Version;
use crate::ring::RawRingMem;
use crate::ring::RecvRing;
use crate::ring::SendRing;
use alloc::vec::Vec;
use core::alloc::Layout;
use opentmk_core::context::HypercallPlatformTrait;
use opentmk_core::platform::hyperv::ctx::HyperVHypercallConfig;

/// Size of the wire [`crate::protocol::PacketDescriptor`], and hence
/// the length of the caller-supplied descriptor accepted by
/// [`RawChannel::send_raw_packet`].
pub const PACKET_DESCRIPTOR_SIZE: usize = 16;

/// Default bounded poll count for [`post_raw_message_wait`].
///
/// Much shorter than [`crate::interrupt::SimpPump::DEFAULT_MAX_RETRIES`]
/// (100M): most fuzzed messages draw no reply at all, so this bound is
/// hit on the majority of `*_comp` calls and dominates testcase
/// latency. ~1M spin-loops is tens of milliseconds — long enough for a
/// host that *is* going to answer, short enough to keep throughput up.
pub const DEFAULT_WAIT_POLLS: usize = 1_000_000;

/// Data pages per ring half in a [`RawChannel`]. Must be a power of
/// two; 8 pages (32 KiB) matches the netvsp driver's ring sizing and
/// is ample for the small packets the fuzzer emits.
const RING_DATA_PAGES: usize = 8;
/// Bytes of ring data per half.
const RING_DATA_BYTES: usize = RING_DATA_PAGES * 4096;

/// Resolve a fuzzer-supplied connection id to the one to post on.
///
/// Mirrors the legacy puppet `setup_msg_and_conn_id`:
///
/// * `0` means "whatever the real client would use" — the id the host
///   handed back during negotiation, falling back to the
///   version-appropriate default (legacy `1` below 5.0, modern `4` at
///   or above) when nothing has been negotiated yet.
/// * Anything else is taken literally, masked to the 24 bits the
///   hypervisor actually uses for a connection id. Letting the fuzzer
///   pick here is deliberate: posting on a connection id the guest
///   never negotiated is a real attack shape.
pub fn resolve_connection_id(requested: u64) -> u32 {
    if requested != 0 {
        return (requested & 0xff_ffff) as u32;
    }
    match connection().as_ref() {
        Some(state) => state.post_message_connection_id,
        // Not yet negotiated: pick the default for the newest version
        // we would ask for, matching `initial_connection_id`.
        None => {
            if Version::NEGOTIATION_LADDER
                .first()
                .is_some_and(|v| *v < Version::Win10Rs3_1)
            {
                VMBUS_CONNECTION_ID_LEGACY
            } else {
                VMBUS_CONNECTION_ID_MODERN
            }
        }
    }
}

/// Post `payload` verbatim as a VMBus channel message.
///
/// No encoding, no validation, no completion registration: the bytes
/// are whatever the fuzzer produced, starting at what the host will
/// read as a [`crate::protocol::MessageHeader`]. Payloads longer than
/// [`MAX_MESSAGE_SIZE`] are truncated rather than rejected so the host
/// still gets to parse the leading bytes.
pub fn post_raw_message<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
    ctx: &mut C,
    connection_id: u32,
    payload: &[u8],
) -> Result<()> {
    let len = payload.len().min(MAX_MESSAGE_SIZE);
    post_message(ctx, connection_id, &payload[..len])
}

/// Post `payload` verbatim, then drain the message page for up to
/// `max_polls` iterations and return the first message the host sends
/// back.
///
/// Returns `Ok(None)` if nothing arrived within the bound, which is
/// the common case — most channel messages are fire-and-forget and the
/// host answers only a handful of request types.
///
/// Messages are still routed through the completion table and the
/// offer sink on the way past, so a reply that a *different* part of
/// the guest was waiting on is not swallowed by this call.
pub fn post_raw_message_wait<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
    ctx: &mut C,
    connection_id: u32,
    payload: &[u8],
    max_polls: usize,
) -> Result<Option<Vec<u8>>> {
    post_raw_message(ctx, connection_id, payload)?;
    poll_capture(ctx, max_polls)
}

/// Drain and discard up to `max_polls` pending messages.
///
/// Used at testcase boundaries: a testcase that provoked replies
/// nobody consumed would otherwise leave them in the SIMP slot, where
/// the *next* testcase's `*_comp` call would pick them up and report
/// them as its own response.
///
/// Returns the number of messages drained.
pub fn drain_pending<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
    ctx: &mut C,
    max_polls: usize,
) -> Result<usize> {
    let mut drained = 0;
    for _ in 0..max_polls {
        match poll_capture(ctx, 1)? {
            Some(_) => drained += 1,
            None => break,
        }
    }
    Ok(drained)
}

/// Relative ids (`child_relid`) of the offers the host has delivered.
///
/// Backs the grammar's `fill_relids` call, which seeds the fuzzer's
/// argument pool with ids the host actually knows about — without it
/// essentially every relid the fuzzer invents is rejected before
/// reaching any interesting host code.
pub fn relids(offers: &[OfferChannel]) -> Vec<u32> {
    offers.iter().map(|o| o.channel_id.0).collect()
}

/// A VMBus channel opened purely so the fuzzer has a ring to write to.
///
/// Unlike the device drivers in [`crate::devices`], this performs no
/// protocol negotiation after the channel opens — it exists to carry
/// arbitrary ring packets at whatever device is on the other end.
pub struct RawChannel {
    channel: Channel,
    send: SendRing<RawRingMem>,
    recv: RecvRing<RawRingMem>,
    /// Base of the single allocation backing both rings. Freed by
    /// [`Self::close`]; leaked if the value is simply dropped, since
    /// the host may still hold the GPADL.
    ring_base: *mut u8,
    ring_layout: Layout,
    next_transaction_id: u64,
}

impl RawChannel {
    /// Open `offer` with a fresh pair of rings.
    ///
    /// Requires [`crate::init`] to have completed. The channel is
    /// opened with the offer's own connection id / channel id, exactly
    /// as a real driver would, so that only the *traffic* is fuzzed
    /// and the channel itself stays usable across a testcase.
    pub fn open<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        ctx: &mut C,
        offer: &OfferChannel,
    ) -> Result<Self> {
        // Layout mirrors `Netvsp::open`:
        //   page  0          : send control page
        //   pages 1..=N      : send data
        //   page  1+N        : recv control page
        //   pages 2+N..=1+2N : recv data
        const TOTAL_PAGES: usize = 2 * (1 + RING_DATA_PAGES);
        const REGION_BYTES: usize = TOTAL_PAGES * 4096;

        let layout = Layout::from_size_align(REGION_BYTES, 4096).map_err(|_| Error::Parse {
            ty: None,
            reason: "raw channel ring layout invalid",
        })?;
        // SAFETY: size and alignment validated above; the region is
        // freed only by `close`, after the host has torn down the
        // GPADL that points at it.
        #[expect(unsafe_code, reason = "raw page-aligned allocation for GPADL")]
        let base = unsafe { alloc::alloc::alloc_zeroed(layout) };
        if base.is_null() {
            return Err(Error::Parse {
                ty: None,
                reason: "raw channel ring allocation failed",
            });
        }

        let base_gpa = crate::virt_to_phys(base);
        log::debug!("raw open: alloc base_gpa={base_gpa:#x} pages={TOTAL_PAGES}");
        let mut pfns: Vec<u64> = Vec::with_capacity(TOTAL_PAGES);
        for i in 0..TOTAL_PAGES {
            pfns.push((base_gpa + (i * 4096) as u64) >> 12);
        }

        let gpadl = match establish_gpadl(ctx, offer.channel_id, REGION_BYTES as u32, &pfns) {
            Ok(g) => {
                log::debug!("raw open: gpadl established handle={g:?}");
                g
            }
            Err(e) => {
                // SAFETY: `base` came from `alloc_zeroed(layout)` and
                // no GPADL references it (the call above failed).
                #[expect(unsafe_code, reason = "free the allocation we just made")]
                unsafe {
                    alloc::alloc::dealloc(base, layout)
                };
                return Err(e);
            }
        };

        let send_data_off = 4096usize;
        let recv_ctrl_off = send_data_off + RING_DATA_BYTES;
        let recv_data_off = recv_ctrl_off + 4096;

        // SAFETY: `base` is a live, exclusively-owned, page-aligned
        // region of `REGION_BYTES`; both offsets and `RING_DATA_BYTES`
        // (a power of two) stay inside it, and the ring types only
        // ever touch the memory atomically.
        #[expect(unsafe_code, reason = "raw ring memory over identity-mapped region")]
        let (send_mem, recv_mem) = unsafe {
            (
                RawRingMem::new(
                    base as *const core::sync::atomic::AtomicU32,
                    base.add(send_data_off) as *const core::sync::atomic::AtomicU8,
                    RING_DATA_BYTES,
                ),
                RawRingMem::new(
                    base.add(recv_ctrl_off) as *const core::sync::atomic::AtomicU32,
                    base.add(recv_data_off) as *const core::sync::atomic::AtomicU8,
                    RING_DATA_BYTES,
                ),
            )
        };

        let channel = match open_channel(
            ctx,
            offer,
            gpadl,
            RING_DATA_PAGES as u32,
            offer.connection_id,
            offer.channel_id.0 as u16,
        ) {
            Ok(c) => {
                log::debug!("raw open: open_channel done");
                c
            }
            Err(e) => {
                // Release the GPADL before freeing the pages it names,
                // otherwise the host keeps a mapping to memory the
                // allocator is free to hand out again.
                if let Err(te) = teardown_gpadl(ctx, gpadl) {
                    log::warn!("raw channel: gpadl teardown after failed open: {te:?}");
                }
                // SAFETY: as above; the GPADL is gone and the channel
                // never opened, so nothing else references `base`.
                #[expect(unsafe_code, reason = "free the allocation we just made")]
                unsafe {
                    alloc::alloc::dealloc(base, layout)
                };
                return Err(e);
            }
        };

        Ok(Self {
            channel,
            send: SendRing::new(send_mem),
            recv: RecvRing::new(recv_mem),
            ring_base: base,
            ring_layout: layout,
            next_transaction_id: 1,
        })
    }

    /// The underlying channel handle.
    pub fn channel(&self) -> &Channel {
        &self.channel
    }

    /// Allocate the next transaction id for a packet that requests a
    /// completion.
    pub fn next_transaction_id(&mut self) -> u64 {
        let id = self.next_transaction_id;
        self.next_transaction_id = self.next_transaction_id.wrapping_add(1);
        id
    }

    /// Send a well-formed packet of arbitrary `packet_type`, letting
    /// the ring compute the descriptor's geometry.
    ///
    /// This is the `pkt_desc == NULL` arm of the legacy puppet
    /// `VmbusPacketMessage` handler: the fuzzer picks the packet type
    /// and payload, but the descriptor stays self-consistent so the
    /// host parses it and reaches the type-specific handling.
    pub fn send_packet<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        &mut self,
        ctx: &mut C,
        packet_type: PacketType,
        payload: &[u8],
        flags: PacketFlags,
    ) -> Result<()> {
        let transaction_id = self.next_transaction_id();
        let signal = self
            .send
            .write_packet(packet_type, &[], payload, flags, transaction_id)?;
        if signal {
            self.channel.signal(ctx)?;
        }
        Ok(())
    }

    /// Send a packet whose descriptor bytes come straight from the
    /// fuzzer.
    ///
    /// This is the interesting arm: `descriptor` is written to the
    /// ring verbatim, so `length8` / `data_offset8` / `flags` can
    /// describe a packet that has nothing to do with the bytes that
    /// follow it. See [`SendRing::write_raw_packet`] for why the
    /// guest's own ring accounting ignores those fields.
    pub fn send_raw_packet<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        &mut self,
        ctx: &mut C,
        descriptor: &[u8; PACKET_DESCRIPTOR_SIZE],
        payload: &[u8],
    ) -> Result<()> {
        let signal = self.send.write_raw_packet(descriptor, payload)?;
        if signal {
            self.channel.signal(ctx)?;
        }
        Ok(())
    }

    /// Drain up to `max_packets` inbound packets, discarding them.
    ///
    /// The fuzzer never inspects what the device sends back, but it
    /// must keep reading: an undrained recv ring fills up, and the
    /// host then stops making forward progress on the send side too.
    pub fn drain_recv(&mut self, max_packets: usize) -> usize {
        let mut buf = alloc::vec![0u8; RING_DATA_BYTES.min(64 * 1024)];
        let mut count = 0;
        for _ in 0..max_packets {
            match self.recv.read(&mut buf) {
                Ok(_) => count += 1,
                // `Parse` here means the *host* wrote a descriptor we
                // can't make sense of. Stop rather than spin: the read
                // index hasn't advanced, so retrying loops forever.
                Err(_) => break,
            }
        }
        count
    }

    /// Close the channel, tear down its ring GPADL, and free the ring
    /// memory.
    ///
    /// Ordering matters and is the same as the netvsp reset path: the
    /// host must stop touching the ring (close) and give up its
    /// mapping (teardown) before the pages go back to the allocator,
    /// or the next `open` over the same PFNs is NAKed.
    pub fn close<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        self,
        ctx: &mut C,
    ) -> Result<()> {
        let Self {
            channel,
            ring_base,
            ring_layout,
            ..
        } = self;
        let ring_gpadl = channel.ring_gpadl();

        let close_res = close_channel(ctx, channel);
        if let Err(e) = &close_res {
            log::warn!("raw channel: close_channel failed: {e:?}");
        }
        if let Err(e) = teardown_gpadl(ctx, ring_gpadl) {
            log::warn!("raw channel: ring gpadl teardown failed: {e:?}");
        }

        // SAFETY: `ring_base`/`ring_layout` are the pair returned by
        // `alloc_zeroed` in `open`, and both the channel and its GPADL
        // have been released above, so the host no longer maps it.
        #[expect(unsafe_code, reason = "free the ring region allocated in `open`")]
        unsafe {
            alloc::alloc::dealloc(ring_base, ring_layout)
        };

        close_res
    }
}

// SAFETY: `RawChannel` holds raw pointers into a region it owns
// exclusively. The guest fuzz executor is single-threaded and only
// ever touches the channel while holding the session mutex, matching
// the rationale for `Netvsp`'s equivalent wrapper.
#[expect(unsafe_code, reason = "raw-pointer-holding session stored in a static")]
unsafe impl Send for RawChannel {}

/// Drain the SIMP page until a message arrives or `max_polls` is
/// exhausted, returning the raw bytes of the first message seen.
#[cfg(target_os = "uefi")]
fn poll_capture<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
    ctx: &mut C,
    max_polls: usize,
) -> Result<Option<Vec<u8>>> {
    use crate::connection::OfferCollector;
    use crate::interrupt::drain_once_capture;
    use crate::interrupt::slot_offset;
    use crate::synic::VMBUS_SINT;
    use crate::synic::synic_pages;
    use core::hint::spin_loop;
    use hvdef::HV_MESSAGE_SIZE;

    let pages = synic_pages().ok_or(Error::VersionMismatch)?;
    let table = completion_table();
    let mut sink = OfferCollector::default();

    for _ in 0..max_polls {
        // SAFETY: `simp_gpa` is a live guest page programmed into SIMP
        // by `crate::synic::init_synic`; guest memory is identity
        // mapped under UEFI so it can be addressed as a raw slice.
        #[expect(unsafe_code, reason = "raw SIMP page access")]
        let slot = unsafe {
            core::slice::from_raw_parts_mut(
                (pages.simp_gpa as *mut u8).add(slot_offset(VMBUS_SINT)),
                HV_MESSAGE_SIZE,
            )
        };
        if let Some(bytes) = drain_once_capture(ctx, slot, table, &mut sink)? {
            return Ok(Some(bytes));
        }
        spin_loop();
    }
    Ok(None)
}

/// Host-target stub: there is no SIMP page to drain off-UEFI.
#[cfg(not(target_os = "uefi"))]
fn poll_capture<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
    _ctx: &mut C,
    _max_polls: usize,
) -> Result<Option<Vec<u8>>> {
    Err(Error::NotImplemented)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerocopy::FromZeros;

    #[test]
    fn resolve_connection_id_masks_to_24_bits() {
        assert_eq!(resolve_connection_id(0xdead_beef_u64), 0xad_beef);
        assert_eq!(resolve_connection_id(4), 4);
        assert_eq!(resolve_connection_id(1), 1);
    }

    #[test]
    fn resolve_connection_id_zero_uses_default_without_connection() {
        // No negotiation has happened in a unit-test process, so the
        // version-appropriate default is used. The ladder starts at
        // Copper (6.0), i.e. the modern id.
        assert_eq!(resolve_connection_id(0), VMBUS_CONNECTION_ID_MODERN);
    }

    #[test]
    fn relids_projects_channel_ids() {
        let mut a = OfferChannel::new_zeroed();
        a.channel_id = crate::protocol::ChannelId(7);
        let mut b = OfferChannel::new_zeroed();
        b.channel_id = crate::protocol::ChannelId(9);
        assert_eq!(relids(&[a, b]), alloc::vec![7, 9]);
    }
}
