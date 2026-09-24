// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hyper-V synthetic NIC vdev — the
//! `f8615163-df3e-46c5-913f-f2d2f965ed0e` VMBus device.
//!
//! Protocol constants and wire types are cross-checked against:
//! - Windows `nvspprotocol.h` (via bluebird
//!   `os2/publics:amd64/onecore/internal/vm/inc/nvspprotocol.h`),
//! - Linux `drivers/net/hyperv/hyperv_net.h`,
//! - openvmm `vm/devices/net/netvsp/src/protocol.rs`,
//! - puppet `kernel/shared/src/nvsc/ty.rs`.
//!
//! # Bring-up sequence
//!
//! The canonical sequence to go from an offer to a working data path
//! (matches Linux `netvsc_connect_vsp` and the Windows NVSC
//! driver):
//!
//! 1. [`Netvsp::open`] — allocate ring pages, establish the ring
//!    GPADL, open the channel with polling target VP.
//! 2. [`Netvsp::negotiate_version`] — walk [`NEGOTIATION_LADDER`]
//!    high → low. `INIT` messages always use
//!    [`NVSP_LEGACY_MESSAGE_SIZE`] regardless of version because we
//!    haven't negotiated yet.
//! 3. [`Netvsp::send_ndis_config`] — V2+ only, fire-and-forget.
//! 4. [`Netvsp::send_ndis_version`] — fire-and-forget.
//! 5. [`Netvsp::establish_recv_buffer`] — host writes RX packets
//!    into this buffer and delivers pointers via xfer-page packets.
//! 6. [`Netvsp::establish_send_buffer`] — optional send-buffer
//!    section allocator for RNDIS packets that fit in a section.
//! 7. [`Netvsp::rndis_init`] — RNDIS `Initialize` handshake.
//! 8. [`Netvsp::set_packet_filter`] — **required for RX**. NDIS's
//!    default filter is 0; the vSwitch silently drops every frame
//!    until we opt in via `OID_GEN_CURRENT_PACKET_FILTER`.
//! 9. [`Netvsp::send_ethernet`] / [`Netvsp::drain_inbound`] —
//!    normal data path.
//!
//! # End-to-end example
//!
//! ```ignore
//! use vmbus_guest::devices::netvsp::{self, Netvsp, rndis};
//!
//! // 1. Pick the netvsp offer out of the enumerated list.
//! let offer = offers
//!     .iter()
//!     .find(|o| o.interface_id == netvsp::INTERFACE_GUID)
//!     .ok_or(vmbus_guest::Error::NotFound)?;
//!
//! // 2. Bring the channel up all the way to a working data path.
//! let mut nic = Netvsp::open(&mut ctx, offer)?;
//! let version = nic.negotiate_version(&mut ctx)?;
//! log::info!("netvsp: negotiated version {:#x}", version);
//! nic.send_ndis_config(&mut ctx, /*mtu=*/ 1500)?;
//! nic.send_ndis_version(&mut ctx)?;
//! nic.establish_recv_buffer(&mut ctx, 16 * 1024 * 1024)?;
//! nic.establish_send_buffer(&mut ctx,  1 * 1024 * 1024)?;
//! nic.rndis_init(&mut ctx)?;
//!
//! // Matches Linux rndis_filter_open. PROMISCUOUS is only needed if
//! // the ARP source MAC we send doesn't match the vNIC's assigned MAC.
//! let filter = rndisprot::NDIS_PACKET_TYPE_DIRECTED
//!     | rndisprot::NDIS_PACKET_TYPE_BROADCAST
//!     | rndisprot::NDIS_PACKET_TYPE_ALL_MULTICAST
//!     | rndis_extras::NDIS_PACKET_TYPE_PROMISCUOUS;
//! nic.set_packet_filter(&mut ctx, filter)?;
//!
//! // 3a. Round-trip send — waits for the paired VM_PKT_COMP and
//! //     frees the RNDIS buffer before returning.
//! nic.send_ethernet(&mut ctx, &arp_request, /*wait=*/ true)?;
//! nic.drain_inbound(&mut ctx, /*max_polls=*/ 10_000_000, |frame| {
//!     log::info!("rx {} bytes", frame.len());
//! })?;
//!
//! // 3b. Fire-and-forget stress burst.
//! for _ in 0..1024 {
//!     match nic.send_ethernet(&mut ctx, &eth, /*wait=*/ false) {
//!         Ok(()) => {}
//!         Err(vmbus_guest::Error::RingFull) => {
//!             nic.drain_inbound(&mut ctx, 100_000, |_| {})?;
//!             nic.send_ethernet(&mut ctx, &eth, false)?;
//!         }
//!         Err(e) => return Err(e),
//!     }
//! }
//! // Flush before returning: drains every outstanding TX completion
//! // and frees the backing allocations. Without this, TX buffers
//! // registered with the pending_tx tracker leak at drop.
//! nic.flush_tx(&mut ctx, /*max_polls=*/ 10_000_000, |_| {})?;
//! # Ok::<_, vmbus_guest::Error>(())
//! ```
//!
//! # Gotchas
//!
//! * **Always drain the recv ring under sustained TX bursts.**
//!   Every fire-and-forget send leaves a `pending_tx` entry until its
//!   `VM_PKT_COMP` is observed on the recv ring. If the caller never
//!   drains, the recv ring fills up (32 KiB per direction), the host
//!   stops posting completions, and the outstanding heap grows
//!   unbounded until [`Netvsp::send_ethernet`] starts returning
//!   [`crate::Error::RingFull`] at the internal `PENDING_TX_MAX`
//!   cap. Interleave [`Netvsp::drain_inbound`] periodically, and
//!   always call [`Netvsp::flush_tx`] before returning from a burst.
//! * **`set_packet_filter` is not optional.** Skipping it produces a
//!   NIC that can send but never receives.
//! * **`send_ndis_config` is V2+ only** (fire-and-forget). On V1 the
//!   host rejects it; the negotiation ladder already skips this on V1.

use crate::Error;
use crate::Result;
use crate::channel::Channel;
use crate::channel::ChannelState;
use crate::channel::open_channel;
use crate::gpadl::GpadlHandle;
use crate::gpadl::establish_gpadl;
use crate::protocol::PacketType;
use crate::protocol::TransferPageHeader;
use crate::protocol::TransferPageRange;
use crate::ring::PacketFlags;
use crate::ring::RawRingMem;
use crate::ring::RecvRing;
use crate::ring::SendRing;
use crate::virt_to_phys;
use alloc::alloc::alloc_zeroed;
use alloc::alloc::dealloc;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::hint::spin_loop;
use core::ops::BitOr;
use core::ops::BitOrAssign;
use core::ptr::copy_nonoverlapping;
use core::slice::from_raw_parts;
use core::sync::atomic::AtomicU8;
use core::sync::atomic::AtomicU32;
use guid::Guid;
use netvsp_protocol::protocol as nvsp;
use netvsp_protocol::protocol::Status;
use netvsp_protocol::rndisprot;
use opentmk_core::context::HypercallPlatformTrait;
use opentmk_core::platform::hyperv::ctx::HyperVHypercallConfig;
use vmbus_core::protocol::ChannelId;
use vmbus_core::protocol::OfferChannel;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// `f8615163-df3e-46c5-913f-f2d2f965ed0e` — VMBus interface GUID for
/// the Hyper-V synthetic NIC.
pub const INTERFACE_GUID: Guid = Guid {
    data1: 0xf8615163,
    data2: 0xdf3e,
    data3: 0x46c5,
    data4: [0x91, 0x3f, 0xf2, 0xd2, 0xf9, 0x65, 0xed, 0x0e],
};

// ---------------------------------------------------------------------
// Protocol versions
// ---------------------------------------------------------------------

const fn make_version(major: u16, minor: u16) -> u32 {
    ((major as u32) << 16) | minor as u32
}

/// NVSP protocol version. Values match `NVSP_PROTOCOL_VERSION_*` in
/// Windows `nvspprotocol.h`.
///
/// V3 is intentionally absent — never shipped.
#[repr(u32)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum Version {
    /// NVSP 0.2, the original protocol version.
    V1 = make_version(0, 2),
    /// NVSP 3.2, conventionally called version 2.
    V2 = make_version(3, 2),
    /// NVSP 4.0.
    V4 = make_version(4, 0),
    /// NVSP 5.0.
    V5 = make_version(5, 0),
    /// NVSP 6.0.
    V6 = make_version(6, 0),
    /// NVSP 6.1.
    V61 = make_version(6, 1),
}

/// Version-negotiation ladder, highest-first. Iterate top→bottom
/// and stop on the first `InitComplete` with `status = SUCCESS`.
/// Matches Linux `netvsc_connect_vsp`'s `ver_list` (iterated in
/// reverse there but same set).
pub const NEGOTIATION_LADDER: [Version; 6] = [
    Version::V61,
    Version::V6,
    Version::V5,
    Version::V4,
    Version::V2,
    Version::V1,
];

/// Sentinel written to `Netvsp::version` before negotiation succeeds.
pub const INVALID_PROTOCOL_VERSION: u32 = 0xFFFF_FFFF;

// ---------------------------------------------------------------------
// Wire-frame sizes
// ---------------------------------------------------------------------

/// Total NVSP wire-frame size for pre-V6.1 messages. Header + body
/// tail-padded to this length regardless of the actual body size.
pub const NVSP_LEGACY_MESSAGE_SIZE: usize = 0x1c; // 28

/// Total NVSP wire-frame size for V6.1+ messages.
pub const NVSP_V61_MESSAGE_SIZE: usize = 0x28; // 40

/// Frame size to use for the currently-negotiated version.
pub const fn frame_size_for(version: Version) -> usize {
    match version {
        Version::V61 => NVSP_V61_MESSAGE_SIZE,
        _ => NVSP_LEGACY_MESSAGE_SIZE,
    }
}

// ---------------------------------------------------------------------
// Message-type identifiers
// ---------------------------------------------------------------------

/// NVSP message-type discriminator. Written as the first `u32` of
/// every message on the wire.
pub mod msg_type {
    #![expect(missing_docs, reason = "documented at struct level")]

    pub const NONE: u32 = 0;

    // Init messages.
    pub const INIT: u32 = 1;
    pub const INIT_COMPLETE: u32 = 2;

    pub const VERSION_MSG_START: u32 = 100;

    // Version 1 messages.
    pub const V1_SEND_NDIS_VERSION: u32 = 100;
    pub const V1_SEND_RECV_BUF: u32 = 101;
    pub const V1_SEND_RECV_BUF_COMPLETE: u32 = 102;
    pub const V1_REVOKE_RECV_BUF: u32 = 103;
    pub const V1_SEND_SEND_BUF: u32 = 104;
    pub const V1_SEND_SEND_BUF_COMPLETE: u32 = 105;
    pub const V1_REVOKE_SEND_BUF: u32 = 106;
    pub const V1_SEND_RNDIS_PKT: u32 = 107;
    pub const V1_SEND_RNDIS_PKT_COMPLETE: u32 = 108;

    // Version 2 messages (only NDIS config is relevant pre-phase-3).
    pub const V2_SEND_NDIS_CONFIG: u32 = 125;

    // Version 4 messages.
    pub const V4_SEND_VF_ASSOCIATION: u32 = 128;
    pub const V4_SWITCH_DATA_PATH: u32 = 129;

    // Version 5 messages.
    pub const V5_SEND_INDIRECTION_TABLE: u32 = 134;
}

// ---------------------------------------------------------------------
// Buffer id constants
// ---------------------------------------------------------------------

/// Guest-chosen ID for the receive buffer. Value from puppet /
/// convention.
pub const NETVSC_RECEIVE_BUFFER_ID: u16 = 0xcafe;

/// Guest-chosen ID for the send buffer.
pub const NETVSC_SEND_BUFFER_ID: u16 = 0x0;

/// Sentinel meaning "not using a send-buffer section" in
/// `Nvsp1SendRndisPacket::send_buffer_section_index`. External data
/// (GPA-direct) is being used instead.
pub const NETVSC_INVALID_INDEX: u32 = 0xFFFF_FFFF;

/// Minimum accepted section size in both send and receive buffer
/// completions — smaller than a legal Ethernet MTU is nonsense.
pub const NETVSC_MTU_MIN: u32 = 68;

/// Cap on the receive buffer for hosts speaking V1/V2 (15 MiB).
/// Modern hosts can accept up to ~2 GiB but there's no reason to
/// exceed 16 MiB for our smoke tests.
pub const NETVSC_RECEIVE_BUFFER_SIZE_LEGACY: usize = 15 * 1024 * 1024;

// ---------------------------------------------------------------------
// RNDIS channel-type constants (used inside Nvsp1SendRndisPacket)
// ---------------------------------------------------------------------

/// RNDIS data channel type (RMC_DATA).
pub const RMC_DATA: u32 = 0;

/// RNDIS control channel type (RMC_CONTROL) — used for init /
/// query / set.
pub const RMC_CONTROL: u32 = 1;

// ---------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------

/// NDIS capability bits for [`nvsp::Message2SendNdisConfig::capabilities`].
///
/// Bit positions match Windows `NVSP_2_NETVSC_CAPABILITIES`. Bit 4
/// (`correlation_id`) is intentionally never set from the guest per
/// Windows source comment "this capability has never worked
/// correctly, since day 1".
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct NdisCapabilities(pub u64);

impl NdisCapabilities {
    #![expect(missing_docs, reason = "bit accessors")]

    pub const VMQ: u64 = 1 << 0;
    pub const CHIMNEY: u64 = 1 << 1;
    pub const SRIOV: u64 = 1 << 2;
    pub const IEEE_8021Q: u64 = 1 << 3;
    // Bit 4 (CorrelationIdBroken): must always be 0 on guest.
    pub const TEAMING: u64 = 1 << 5;
    pub const VIRTUAL_SUBNET_ID: u64 = 1 << 6;
    pub const RSC_OVER_VMBUS: u64 = 1 << 7;
    pub const TIMESTAMP: u64 = 1 << 8;
    pub const RELIABLE_CORRELATION_ID: u64 = 1 << 9;
    pub const ALLOW_RSC_DISABLED_STATUS: u64 = 1 << 10;

    /// Recommended capability set as a function of negotiated
    /// version. Mirrors Linux's
    /// `negotiate_nvsp_ver` capability construction.
    pub fn recommended(version: Version) -> Self {
        let mut caps = Self::IEEE_8021Q;
        if version >= Version::V5 {
            caps |= Self::SRIOV | Self::TEAMING;
        }
        if version >= Version::V61 {
            caps |= Self::RSC_OVER_VMBUS;
        }
        Self(caps)
    }
}

impl BitOr<u64> for NdisCapabilities {
    type Output = Self;
    fn bitor(self, rhs: u64) -> Self {
        Self(self.0 | rhs)
    }
}

impl BitOrAssign<u64> for NdisCapabilities {
    fn bitor_assign(&mut self, rhs: u64) {
        self.0 |= rhs;
    }
}

// ---------------------------------------------------------------------
// RNDIS wire types (Phase 3)
// ---------------------------------------------------------------------

/// Guest-side RNDIS extras that don't have a canonical home in
/// [`netvsp_protocol::rndisprot`].
///
/// * [`rndis_extras::MAX_TRANSFER_SIZE`] — guest-picked ceiling
///   (16 KiB, matches puppet and Linux drivers).
/// * [`rndis_extras::NDIS_PACKET_TYPE_PROMISCUOUS`] — upstream
///   lists the other filter bits but not this one.
pub mod rndis_extras {
    /// Max transfer size we request in `InitializeRequest`. 16 KiB
    /// matches puppet + Linux.
    pub const MAX_TRANSFER_SIZE: u32 = 0x4000;

    /// Accept every frame regardless of MAC. Upstream omits this
    /// bit; MS-RNDIS §2.2 defines it as 0x0020.
    pub const NDIS_PACKET_TYPE_PROMISCUOUS: u32 = 0x0020;
}

/// Encode a NVSP message: header + body copied into a
/// zero-padded fixed-size frame ([`frame_size_for`]).
///
/// Returns `Err(())` if the header + body exceed the frame size.
/// Frame size 40 (V6.1) accepts any body ≤ 36 bytes; frame size 28
/// (legacy) accepts any body ≤ 24 bytes.
pub fn encode_message<T: IntoBytes + Immutable>(
    message_type: u32,
    body: &T,
    version: Version,
    out: &mut [u8],
) -> core::result::Result<usize, ()> {
    let frame_size = frame_size_for(version);
    if out.len() < frame_size {
        return Err(());
    }
    let hdr = nvsp::MessageHeader { message_type };
    let hdr_bytes = hdr.as_bytes();
    let body_bytes = body.as_bytes();
    if hdr_bytes.len() + body_bytes.len() > frame_size {
        return Err(());
    }
    out[..hdr_bytes.len()].copy_from_slice(hdr_bytes);
    out[hdr_bytes.len()..hdr_bytes.len() + body_bytes.len()].copy_from_slice(body_bytes);
    // Zero-fill the tail — hosts expect a stable frame size.
    for b in &mut out[hdr_bytes.len() + body_bytes.len()..frame_size] {
        *b = 0;
    }
    Ok(frame_size)
}

/// Parse an inbound NVSP frame's header and return
/// `(message_type, body_slice)`.
///
/// The body slice may include trailing padding bytes that the
/// sender zero-filled to reach [`frame_size_for`]; callers should
/// use `zerocopy::FromBytes::read_from_prefix` on the body and
/// ignore the tail.
pub fn parse_header(frame: &[u8]) -> core::result::Result<(u32, &[u8]), ()> {
    let (hdr, rest) = nvsp::MessageHeader::read_from_prefix(frame).map_err(|_| ())?;
    Ok((hdr.message_type, rest))
}

// ---------------------------------------------------------------------
// Netvsp handle (Phase 1: open + negotiate + NDIS config/version)
// ---------------------------------------------------------------------

/// Owned page-aligned buffer with its GPADL registration.
///
/// Kept alive for the lifetime of the netvsp connection — the host
/// retains references to the underlying pages through the GPADL, so
/// dropping this while the host still owns it would be a use-after-free.
pub struct OwnedBuf {
    /// 4 KiB-aligned base pointer. Identity-mapped, so VA == GPA on
    /// our UEFI target.
    pub ptr: *mut u8,
    /// Size in bytes (multiple of 4096).
    pub len: usize,
    /// Registered GPADL id for this buffer.
    pub gpadl: GpadlHandle,
}

// SAFETY: `OwnedBuf` is only ever accessed by the single-threaded
// UEFI runtime; the pointer is a stable identity-mapped allocation.
#[expect(
    unsafe_code,
    reason = "single-threaded UEFI runtime; identity-mapped GPADL pages"
)]
unsafe impl Send for OwnedBuf {}
#[expect(
    unsafe_code,
    reason = "single-threaded UEFI runtime; identity-mapped GPADL pages"
)]
unsafe impl Sync for OwnedBuf {}

/// Freeable guest allocations extracted from a [`Netvsp`] via
/// [`Netvsp::into_parts`], to be released *after* the owning channel
/// has been closed host-side.
///
/// `Netvsp`'s ring region, GPADL buffers, and pending-TX staging
/// buffers are all raw `alloc_zeroed` allocations with no `Drop`, so
/// simply dropping a `Netvsp` (e.g. via [`Netvsp::into_channel`])
/// leaks them. A close/reopen cycle that reclaims memory must route
/// through `into_parts` + [`NetvspBacking::free`] instead.
pub struct NetvspBacking {
    ring_base: *mut u8,
    ring_layout: Layout,
    /// `[recv_buf, send_buf]` — either may be `None` if that buffer was
    /// never established.
    bufs: [Option<OwnedBuf>; 2],
    pending: VecDeque<PendingTx>,
}

// SAFETY: same rationale as `OwnedBuf`/`Netvsp` — only ever touched by
// the single-threaded UEFI runtime while holding the session mutex.
#[expect(
    unsafe_code,
    reason = "single-threaded UEFI runtime; identity-mapped allocations"
)]
unsafe impl Send for NetvspBacking {}

impl NetvspBacking {
    /// GPADL handles for the established recv/send buffers.
    ///
    /// A close/reopen cycle that frees this backing must tear these
    /// down host-side *before* [`free`](Self::free): the freed guest
    /// pages are typically handed straight back to the next
    /// `alloc_zeroed`, so reopening re-registers a fresh GPADL over the
    /// exact PFNs the host still holds under the old (un-torn-down)
    /// handle, and the host NAKs the new `GpadlHeader`. The ring GPADL
    /// (from [`Channel::ring_gpadl`](crate::channel::Channel::ring_gpadl))
    /// must be torn down for the same reason.
    pub fn buffer_gpadls(&self) -> impl Iterator<Item = GpadlHandle> + '_ {
        self.bufs.iter().flatten().map(|b| b.gpadl)
    }

    /// Deallocate every guest allocation this handle owns: the ring
    /// region, the send/recv GPADL buffers, and any not-yet-completed
    /// TX staging buffers.
    ///
    /// # Safety contract (caller-enforced)
    /// Call only after the owning channel has been closed
    /// (`close_channel`) so the host is no longer reading or writing
    /// these pages; otherwise the host would access freed memory.
    pub fn free(self) {
        let NetvspBacking {
            ring_base,
            ring_layout,
            bufs,
            mut pending,
        } = self;

        // SAFETY: `ring_base`/`ring_layout` are exactly the pointer and
        // layout `alloc_zeroed`'d in `Netvsp::open`, freed once here.
        #[expect(unsafe_code, reason = "free ring region from Netvsp::open")]
        unsafe {
            dealloc(ring_base, ring_layout);
        }

        for buf in bufs.into_iter().flatten() {
            let layout = Layout::from_size_align(buf.len, 4096)
                .expect("GPADL buffer len/align were validated at allocation");
            // SAFETY: `buf.ptr`/`layout` reconstruct the exact request
            // `allocate_gpadl_buffer` made; freed once here.
            #[expect(unsafe_code, reason = "free GPADL buffer")]
            unsafe {
                dealloc(buf.ptr, layout);
            }
        }

        for tx in pending.drain(..) {
            // SAFETY: `tx.ptr`/`tx.layout` are the exact request made
            // in `send_ethernet`; its host completion was never
            // observed, but the channel is closed so the host is done.
            #[expect(unsafe_code, reason = "free pending TX staging buffer")]
            unsafe {
                dealloc(tx.ptr, tx.layout);
            }
        }
    }
}

/// Guest-side handle to an open Hyper-V synthetic NIC channel.
///
/// Not thread-safe on its own; the UEFI runtime is effectively
/// single-threaded. State transitions happen only from the calling
/// thread.
pub struct Netvsp {
    channel: Channel,
    send: SendRing<RawRingMem>,
    recv: RecvRing<RawRingMem>,

    /// Base pointer and layout of the single page-aligned allocation
    /// backing both rings (see [`Netvsp::open`]). Retained so the
    /// region can be reclaimed via [`Netvsp::into_parts`] when the
    /// channel is torn down — the `send`/`recv` [`RawRingMem`] only
    /// hold interior pointers and have no `Drop`, so without this the
    /// region would leak on every close.
    ring_base: *mut u8,
    ring_layout: Layout,

    /// Negotiated NVSP version, or [`INVALID_PROTOCOL_VERSION`] until
    /// [`Self::negotiate_version`] succeeds.
    version: u32,

    /// Fresh id counter for outgoing completion-requested sends.
    /// Starts at 1; 0 is reserved for "no completion".
    next_transaction_id: u64,

    /// Guest-owned receive buffer + GPADL registered with the VSP.
    /// Populated by [`Self::establish_recv_buffer`].
    recv_buf: Option<OwnedBuf>,
    /// `sub_allocation_size` reported by the host in the recv-buf
    /// completion. Non-zero means the recv buffer is live.
    recv_section_size: u32,
    /// Number of receive sub-allocations.
    recv_section_count: u32,

    /// Guest-owned send buffer + GPADL. Populated by
    /// [`Self::establish_send_buffer`].
    send_buf: Option<OwnedBuf>,
    /// `section_size` reported by the host in the send-buf completion.
    send_section_size: u32,
    /// Send-section count = send_buf.len / send_section_size.
    send_section_count: u32,

    /// TX buffers whose paired `V1_SEND_RNDIS_PKT_COMPLETE` we
    /// haven't yet observed. Populated by [`Self::send_ethernet`]
    /// (both wait and fire-and-forget modes) and drained by
    /// [`Self::drain_inbound`] and the completion-wait loop inside
    /// `send_ethernet`. Each entry owns the backing allocation and
    /// is freed via `dealloc` when the matching
    /// completion arrives.
    ///
    /// Bounded at [`PENDING_TX_MAX`] — bursts beyond that must be
    /// interleaved with [`Self::flush_tx`] or [`Self::drain_inbound`].
    pending_tx: VecDeque<PendingTx>,
}

/// A raw pointer + layout for a TX RNDIS buffer that's been handed
/// to the host but whose completion hasn't been observed yet. Not
/// `Send`/`Sync` on purpose — UEFI is single-threaded.
struct PendingTx {
    tid: u64,
    ptr: *mut u8,
    layout: Layout,
}

/// Reasonable default retry budget for ring-buffer completion polling.
/// Roughly a few seconds of spinning on modern hardware.
const DEFAULT_MAX_POLLS: usize = 100_000_000;

/// Completion-poll budget for the fuzzer-facing raw send paths
/// (`send_nvsp_raw`, `send_rndis_raw`, `renew_*_buffer`). Much smaller
/// than [`DEFAULT_MAX_POLLS`] (~0.1s vs ~5s) because malformed fuzz
/// input routinely gets no host completion, and a single testcase may
/// chain many such sends. The agent must answer within the fuzzer's
/// 20s TCP read window, so a large per-send timeout would blow the
/// whole testcase budget on the first few sends.
///
/// Sized from a live-run measurement of the network-backed datapath:
/// genuine completions for well-formed sends arrive with p50 ~23ms and
/// 97% within ~75ms, while a malformed send that gets no completion
/// otherwise spins the whole budget. ~0.1s therefore captures
/// effectively all real acks with margin while cutting the wasted spin
/// on dropped packets (~53% of fuzz sends) by more than half. Missing a
/// late ack is harmless: the packet was already delivered to the host
/// via `post_message`, and the per-testcase channel reset discards any
/// completion still queued behind it.
const FUZZ_SEND_MAX_POLLS: usize = 2_000_000;

/// Soft cap on in-flight TX buffers awaiting completion. Sized to
/// keep the outstanding heap footprint bounded at ~2 MiB (512 × 4 KiB)
/// under a stress burst. When we reach the cap, the send paths first
/// synchronously drain completions and then, if still full,
/// force-reclaim the oldest buffers (see [`PENDING_TX_RECLAIM`]).
const PENDING_TX_MAX: usize = 512;

/// When [`PENDING_TX_MAX`] is hit and draining reaps nothing, this
/// many of the oldest outstanding TX buffers are force-reclaimed to
/// keep the GPA-direct send path alive. Under fuzzing the host
/// silently drops malformed sends, so their tracker entries never
/// receive a completion and would otherwise leak forever, wedging
/// every subsequent send at `RingFull`. Reclaiming a batch (rather
/// than a single entry) amortizes the cost across many sends.
const PENDING_TX_RECLAIM: usize = 64;

/// Ring size: 32 data pages = 128 KiB per direction, power-of-two as
/// required by `RawRingMem::new`. Plus 1 control page → 33 pages per
/// direction, 66 pages (264 KiB) total per channel. Sized generously
/// (vs. the 8-page minimum) so bursty fuzzer testcases posting many
/// fire-and-forget sends don't overflow the outbound ring before the
/// host drains it (`Error::RingFull`).
const RING_DATA_PAGES: usize = 32;
const RING_DATA_BYTES: usize = RING_DATA_PAGES * 4096;

impl Netvsp {
    /// Open the synthetic NIC channel described by `offer`.
    ///
    /// * Allocates a contiguous 18-page ring region.
    /// * Establishes a GPADL over it.
    /// * Opens the channel with `target_vp = u32::MAX` (polling).
    ///
    /// After this call succeeds, [`Self::negotiate_version`] must be
    /// called before any other message is sent.
    pub fn open<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        ctx: &mut C,
        offer: &OfferChannel,
    ) -> Result<Self> {
        // Layout of the ring region, in order:
        //   page  0            : send control page
        //   pages 1..=N        : send data (N = RING_DATA_PAGES, power-of-two)
        //   page  1+N          : recv control page
        //   pages 2+N..=1+2N   : recv data
        const TOTAL_PAGES: usize = 2 * (1 + RING_DATA_PAGES);
        const REGION_BYTES: usize = TOTAL_PAGES * 4096;
        let layout = Layout::from_size_align(REGION_BYTES, 4096).map_err(|_| Error::Parse {
            ty: None,
            reason: "netvsp ring layout invalid",
        })?;
        // SAFETY: alignment and size are validated above. Freed via
        // `NetvspBacking::free` (after channel close) — never dropped
        // implicitly, since the region has no `Drop`.
        #[expect(unsafe_code, reason = "raw page-aligned allocation for GPADL")]
        let base = unsafe { alloc_zeroed(layout) };
        if base.is_null() {
            return Err(Error::Parse {
                ty: None,
                reason: "netvsp ring allocation failed",
            });
        }
        let base_gpa = virt_to_phys(base);
        log::debug!(
            "netvsp: ring region at GPA {:#x} ({} bytes)",
            base_gpa,
            REGION_BYTES,
        );

        let mut pfns: Vec<u64> = Vec::with_capacity(TOTAL_PAGES);
        for i in 0..TOTAL_PAGES {
            pfns.push((base_gpa + (i * 4096) as u64) >> 12);
        }
        let gpadl = establish_gpadl(ctx, offer.channel_id, REGION_BYTES as u32, &pfns)?;
        log::debug!(
            "netvsp: ring GPADL established id={:?} for channel {:?}",
            gpadl.id(),
            offer.channel_id,
        );

        // send data starts at page 1, recv at page 1+RING_DATA_PAGES+1
        // (control + 8 data + recv control page).
        let send_ctrl_off = 0usize;
        let send_data_off = 4096usize;
        let recv_ctrl_off = send_data_off + RING_DATA_BYTES;
        let recv_data_off = recv_ctrl_off + 4096;

        // SAFETY: `alloc_zeroed(REGION_BYTES)` returned a valid
        // contiguous allocation we own for the process lifetime.
        // Ring memory objects will hold only atomic pointers into it.
        #[expect(unsafe_code, reason = "raw ring memory over identity-mapped region")]
        let send_mem = unsafe {
            RawRingMem::new(
                base.add(send_ctrl_off) as *const AtomicU32,
                base.add(send_data_off) as *const AtomicU8,
                RING_DATA_BYTES,
            )
        };
        #[expect(unsafe_code, reason = "raw ring memory over identity-mapped region")]
        let recv_mem = unsafe {
            RawRingMem::new(
                base.add(recv_ctrl_off) as *const AtomicU32,
                base.add(recv_data_off) as *const AtomicU8,
                RING_DATA_BYTES,
            )
        };

        // Open the channel. `send_data_pages` matches our layout so
        // the host knows where the send ring ends and recv begins.
        let channel = open_channel(
            ctx,
            offer,
            gpadl,
            RING_DATA_PAGES as u32,
            offer.connection_id,
            offer.channel_id.0 as u16,
        )?;
        log::debug!("netvsp: channel opened");

        Ok(Self {
            channel,
            send: SendRing::new(send_mem),
            recv: RecvRing::new(recv_mem),
            ring_base: base,
            ring_layout: layout,
            version: INVALID_PROTOCOL_VERSION,
            next_transaction_id: 1,
            recv_buf: None,
            recv_section_size: 0,
            recv_section_count: 0,
            send_buf: None,
            send_section_size: 0,
            send_section_count: 0,
            pending_tx: VecDeque::new(),
        })
    }

    /// Negotiate the NVSP protocol version by walking [`NEGOTIATION_LADDER`]
    /// high→low. Returns the accepted version.
    ///
    /// A single ladder attempt = post `INIT` with completion flag,
    /// wait for `INIT_COMPLETE`. On `status = SUCCESS`, this becomes
    /// the negotiated version; otherwise fall through to the next
    /// ladder entry. Matches Linux's `netvsc_connect_vsp` and
    /// puppet's `negotiate_versions`.
    pub fn negotiate_version<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        &mut self,
        ctx: &mut C,
    ) -> Result<Version> {
        for &v in &NEGOTIATION_LADDER {
            match self.try_init(ctx, v) {
                Ok(true) => {
                    self.version = v as u32;
                    log::debug!("netvsp: negotiated version {:?}", v);
                    return Ok(v);
                }
                Ok(false) => {
                    log::debug!("netvsp: host rejected {:?}, trying next", v);
                    continue;
                }
                Err(Error::Timeout) => {
                    log::debug!("netvsp: {:?} timed out, trying next", v);
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Err(Error::VersionMismatch)
    }

    /// Send `nvsp::MessageInit` for `version` and await `InitComplete`.
    /// Returns `Ok(true)` if the host accepted, `Ok(false)` if the
    /// host returned a non-success status.
    fn try_init<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        &mut self,
        ctx: &mut C,
        version: Version,
    ) -> Result<bool> {
        // nvsp::MessageInit is 8 bytes. `INIT` messages **always** go out
        // as `NVSP_LEGACY_MESSAGE_SIZE (28)` regardless of the
        // requested version — Windows: "Init message has always size
        // of NVSP_LEGACY_MESSAGE_SIZE in order to be able to
        // negotiate with older hosts" (`NetVsc.c` around
        // `NvscSendInitializationMessage`).
        let mut frame = [0u8; NVSP_LEGACY_MESSAGE_SIZE];
        // Use V1 to force the legacy frame length. We're not yet
        // negotiated so `frame_size_for(self.version)` would panic.
        encode_message(
            msg_type::INIT,
            &nvsp::MessageInit {
                protocol_version: version as u32,
                protocol_version2: version as u32,
            },
            Version::V1,
            &mut frame,
        )
        .map_err(|_| Error::Parse {
            ty: None,
            reason: "encode INIT",
        })?;

        let response = self.send_and_await(ctx, &frame, DEFAULT_MAX_POLLS)?;
        let (ty, body) = parse_header(&response).map_err(|_| Error::Parse {
            ty: None,
            reason: "parse INIT_COMPLETE header",
        })?;
        if ty != msg_type::INIT_COMPLETE {
            return Err(Error::Parse {
                ty: None,
                reason: "expected INIT_COMPLETE",
            });
        }
        let (parsed, _) =
            nvsp::MessageInitComplete::read_from_prefix(body).map_err(|_| Error::Parse {
                ty: None,
                reason: "parse INIT_COMPLETE body",
            })?;
        Ok(parsed.status == Status::SUCCESS)
    }

    /// Send `Nvsp2SendNdisConfig` (V2+ only). Fire-and-forget, no
    /// completion expected.
    ///
    /// Ignored if the current negotiated version is V1 (which does not
    /// use NDIS config).
    pub fn send_ndis_config<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        &mut self,
        ctx: &mut C,
        mtu: u32,
    ) -> Result<()> {
        let version = self.version_typed()?;
        if version == Version::V1 {
            return Ok(());
        }
        let caps = NdisCapabilities::recommended(version);
        let mut frame = [0u8; NVSP_V61_MESSAGE_SIZE];
        let n = encode_message(
            msg_type::V2_SEND_NDIS_CONFIG,
            &nvsp::Message2SendNdisConfig {
                mtu,
                reserved: 0,
                capabilities: nvsp::NdisConfigCapabilities::from(caps.0),
            },
            version,
            &mut frame,
        )
        .map_err(|_| Error::Parse {
            ty: None,
            reason: "encode NDIS_CONFIG",
        })?;
        self.send_no_completion(ctx, &frame[..n])
    }

    /// Send `Nvsp1SendNdisVersion`. Fire-and-forget, no completion.
    ///
    /// `major = 6`, `minor = 30` for V5+ or `minor = 1` otherwise —
    /// matches Linux `negotiate_nvsp_ver` and puppet.
    pub fn send_ndis_version<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        &mut self,
        ctx: &mut C,
    ) -> Result<()> {
        let version = self.version_typed()?;
        let ndis_minor = if version <= Version::V4 { 1 } else { 0x1e };
        let mut frame = [0u8; NVSP_V61_MESSAGE_SIZE];
        let n = encode_message(
            msg_type::V1_SEND_NDIS_VERSION,
            &nvsp::Message1SendNdisVersion {
                ndis_major_version: 6,
                ndis_minor_version: ndis_minor,
            },
            version,
            &mut frame,
        )
        .map_err(|_| Error::Parse {
            ty: None,
            reason: "encode NDIS_VERSION",
        })?;
        self.send_no_completion(ctx, &frame[..n])
    }

    /// Establish the receive buffer (host → guest data path).
    ///
    /// Allocates `size` bytes (must be page-multiple), registers a
    /// GPADL for the whole region, sends `V1_SEND_RECV_BUF`, and
    /// waits for `V1_SEND_RECV_BUF_COMPLETE`. Validates:
    /// * `status == SUCCESS`
    /// * `num_sections == 1` (spec quirk: no VSP has ever sent more)
    /// * `sections[0].offset == 0`
    /// * `sub_allocation_size >= NETVSC_MTU_MIN`
    /// * `u64(sub_allocation_size) * u64(num_sub_allocations) <= size`
    ///
    /// A 16 MiB buffer produces ~147 `GpadlBody` messages posted
    /// back-to-back — this is the first real exercise of the
    /// `hypercalls::post_message` retry loop added in a prior commit.
    pub fn establish_recv_buffer<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        &mut self,
        ctx: &mut C,
        size: usize,
    ) -> Result<()> {
        if self.recv_buf.is_some() {
            return Err(Error::Parse {
                ty: None,
                reason: "recv buffer already established",
            });
        }
        let buf = allocate_gpadl_buffer(ctx, self.channel.channel_id(), size)?;
        log::debug!(
            "netvsp: recv-buf allocated {} bytes, GPADL id={:?}",
            size,
            buf.gpadl.id(),
        );

        let mut frame = [0u8; NVSP_V61_MESSAGE_SIZE];
        let n = encode_message(
            msg_type::V1_SEND_RECV_BUF,
            &nvsp::Message1SendReceiveBuffer {
                gpadl_handle: buf.gpadl.id(),
                id: NETVSC_RECEIVE_BUFFER_ID,
                reserved: 0,
            },
            self.version_typed()?,
            &mut frame,
        )
        .map_err(|_| Error::Parse {
            ty: None,
            reason: "encode SEND_RECV_BUF",
        })?;

        let response = self.send_and_await(ctx, &frame[..n], DEFAULT_MAX_POLLS)?;
        let (ty, body) = parse_header(&response).map_err(|_| Error::Parse {
            ty: None,
            reason: "parse SEND_RECV_BUF_COMPLETE header",
        })?;
        if ty != msg_type::V1_SEND_RECV_BUF_COMPLETE {
            return Err(Error::Parse {
                ty: None,
                reason: "expected SEND_RECV_BUF_COMPLETE",
            });
        }
        let (parsed, _) =
            nvsp::Message1SendReceiveBufferComplete::read_from_prefix(body).map_err(|_| {
                Error::Parse {
                    ty: None,
                    reason: "parse SEND_RECV_BUF_COMPLETE body",
                }
            })?;

        if parsed.status != Status::SUCCESS {
            log::warn!(
                "netvsp: recv-buf complete status = {:?} (not SUCCESS)",
                parsed.status
            );
            return Err(Error::Parse {
                ty: None,
                reason: "recv-buf complete non-success status",
            });
        }
        if parsed.num_sections != 1 {
            log::warn!(
                "netvsp: recv-buf num_sections = {} (expected 1)",
                parsed.num_sections
            );
            return Err(Error::Parse {
                ty: None,
                reason: "recv-buf num_sections != 1",
            });
        }
        let sec = &parsed.sections[0];
        if sec.offset != 0 {
            return Err(Error::Parse {
                ty: None,
                reason: "recv-buf section offset != 0",
            });
        }
        if sec.sub_allocation_size < NETVSC_MTU_MIN {
            log::warn!(
                "netvsp: recv-buf sub_allocation_size = {} (< MTU_MIN={})",
                sec.sub_allocation_size,
                NETVSC_MTU_MIN
            );
            return Err(Error::Parse {
                ty: None,
                reason: "recv-buf sub_allocation_size < MTU_MIN",
            });
        }
        let used = (sec.sub_allocation_size as u64) * (sec.num_sub_allocations as u64);
        if used > size as u64 {
            return Err(Error::Parse {
                ty: None,
                reason: "recv-buf sub_allocation_size * count > allocation",
            });
        }
        log::debug!(
            "netvsp: recv-buf established: sub_allocation_size={}, num_sub_allocations={}, used={}/{}",
            sec.sub_allocation_size,
            sec.num_sub_allocations,
            used,
            size,
        );

        self.recv_section_size = sec.sub_allocation_size;
        self.recv_section_count = sec.num_sub_allocations;
        self.recv_buf = Some(buf);
        Ok(())
    }

    /// Establish the send buffer (guest → host bulk data path).
    ///
    /// Same shape as [`Self::establish_recv_buffer`] but for the
    /// `SEND_SEND_BUF` variant. Response validation:
    /// * `status == SUCCESS`
    /// * `section_size >= NETVSC_MTU_MIN`
    /// * `send_section_count = size / section_size > 0`
    pub fn establish_send_buffer<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        &mut self,
        ctx: &mut C,
        size: usize,
    ) -> Result<()> {
        if self.send_buf.is_some() {
            return Err(Error::Parse {
                ty: None,
                reason: "send buffer already established",
            });
        }
        let buf = allocate_gpadl_buffer(ctx, self.channel.channel_id(), size)?;
        log::debug!(
            "netvsp: send-buf allocated {} bytes, GPADL id={:?}",
            size,
            buf.gpadl.id(),
        );

        let mut frame = [0u8; NVSP_V61_MESSAGE_SIZE];
        let n = encode_message(
            msg_type::V1_SEND_SEND_BUF,
            &nvsp::Message1SendSendBuffer {
                gpadl_handle: buf.gpadl.id(),
                id: NETVSC_SEND_BUFFER_ID,
                reserved: 0,
            },
            self.version_typed()?,
            &mut frame,
        )
        .map_err(|_| Error::Parse {
            ty: None,
            reason: "encode SEND_SEND_BUF",
        })?;

        let response = self.send_and_await(ctx, &frame[..n], DEFAULT_MAX_POLLS)?;
        let (ty, body) = parse_header(&response).map_err(|_| Error::Parse {
            ty: None,
            reason: "parse SEND_SEND_BUF_COMPLETE header",
        })?;
        if ty != msg_type::V1_SEND_SEND_BUF_COMPLETE {
            return Err(Error::Parse {
                ty: None,
                reason: "expected SEND_SEND_BUF_COMPLETE",
            });
        }
        let (parsed, _) =
            nvsp::Message1SendSendBufferComplete::read_from_prefix(body).map_err(|_| {
                Error::Parse {
                    ty: None,
                    reason: "parse SEND_SEND_BUF_COMPLETE body",
                }
            })?;
        if parsed.status != Status::SUCCESS {
            return Err(Error::Parse {
                ty: None,
                reason: "send-buf complete non-success status",
            });
        }
        if parsed.section_size < NETVSC_MTU_MIN {
            return Err(Error::Parse {
                ty: None,
                reason: "send-buf section_size < MTU_MIN",
            });
        }
        let count = (size as u32) / parsed.section_size;
        if count == 0 {
            return Err(Error::Parse {
                ty: None,
                reason: "send-buf section_size larger than buffer",
            });
        }
        log::debug!(
            "netvsp: send-buf established: section_size={}, count={}",
            parsed.section_size,
            count,
        );

        self.send_section_size = parsed.section_size;
        self.send_section_count = count;
        self.send_buf = Some(buf);
        Ok(())
    }

    /// The negotiated version, converted back to the typed enum.
    /// Errors if negotiation hasn't happened yet.
    pub fn version_typed(&self) -> Result<Version> {
        match self.version {
            v if v == Version::V1 as u32 => Ok(Version::V1),
            v if v == Version::V2 as u32 => Ok(Version::V2),
            v if v == Version::V4 as u32 => Ok(Version::V4),
            v if v == Version::V5 as u32 => Ok(Version::V5),
            v if v == Version::V6 as u32 => Ok(Version::V6),
            v if v == Version::V61 as u32 => Ok(Version::V61),
            _ => Err(Error::VersionMismatch),
        }
    }

    /// Section size reported by the host for the receive buffer.
    /// Zero until [`Self::establish_recv_buffer`] succeeds.
    pub fn recv_section_size(&self) -> u32 {
        self.recv_section_size
    }

    /// Section size reported by the host for the send buffer.
    /// Zero until [`Self::establish_send_buffer`] succeeds.
    pub fn send_section_size(&self) -> u32 {
        self.send_section_size
    }

    /// Send RNDIS `Initialize` and wait for the paired
    /// `RNDIS_INITIALIZE_COMPLETE`.
    ///
    /// Sequence:
    /// 1. Allocate a page-aligned buffer, write
    ///    `rndisprot::MessageHeader + rndisprot::InitializeRequest`.
    /// 2. Send `V1_SEND_RNDIS_PKT(RMC_CONTROL)` via
    ///    `SendRing::write_gpa_direct` with the RNDIS bytes carried
    ///    as external GPA-direct data. Completion-requested flag set.
    /// 3. Wait for `V1_SEND_RNDIS_PKT_COMPLETE` — acks the NVSP
    ///    resource, NOT the RNDIS init itself.
    /// 4. Wait for the RNDIS response arriving as a
    ///    `VM_PKT_DATA_USING_XFER_PAGES` referencing our recv buffer.
    /// 5. Parse the transfer-page header + range, read
    ///    `rndisprot::InitializeComplete` from recv_buf + range.byte_offset,
    ///    verify `status == STATUS_SUCCESS`.
    /// 6. Send `VM_PKT_COMP` back so the host can free its transfer
    ///    pages.
    ///
    /// Requires [`Self::establish_recv_buffer`] to have succeeded
    /// (we need the recv buffer to receive the completion into).
    pub fn rndis_init<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        &mut self,
        ctx: &mut C,
    ) -> Result<()> {
        let recv_buf = self.recv_buf.as_ref().ok_or(Error::Parse {
            ty: None,
            reason: "rndis_init requires establish_recv_buffer first",
        })?;
        let recv_base = recv_buf.ptr;
        let recv_len = recv_buf.len;

        // Allocate a page-aligned RNDIS message buffer. Total is
        // small (8 + 16 = 24 bytes) but we take a whole page for
        // clean GPA-direct addressing.
        let rndis_layout = Layout::from_size_align(4096, 4096).map_err(|_| Error::Parse {
            ty: None,
            reason: "rndis buffer layout",
        })?;
        // SAFETY: layout is a validated single-page allocation.
        // Kept alive at least until we've received the completion.
        #[expect(unsafe_code, reason = "page-aligned RNDIS message allocation")]
        let rndis_ptr = unsafe { alloc_zeroed(rndis_layout) };
        if rndis_ptr.is_null() {
            return Err(Error::Parse {
                ty: None,
                reason: "rndis buffer alloc failed",
            });
        }

        // Build RNDIS message: header + InitializeRequest.
        let request_id: u32 = 1;
        let hdr = rndisprot::MessageHeader {
            message_type: rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
            message_length: (size_of::<rndisprot::MessageHeader>()
                + size_of::<rndisprot::InitializeRequest>()) as u32,
        };
        let req = rndisprot::InitializeRequest {
            request_id,
            major_version: rndisprot::MAJOR_VERSION,
            minor_version: rndisprot::MINOR_VERSION,
            max_transfer_size: rndis_extras::MAX_TRANSFER_SIZE,
        };
        let hdr_bytes = hdr.as_bytes();
        let req_bytes = req.as_bytes();
        // SAFETY: `rndis_ptr` is a valid 4 KiB allocation and the
        // combined write is 24 bytes.
        #[expect(unsafe_code, reason = "copy RNDIS bytes into own buffer")]
        unsafe {
            copy_nonoverlapping(hdr_bytes.as_ptr(), rndis_ptr, hdr_bytes.len());
            copy_nonoverlapping(
                req_bytes.as_ptr(),
                rndis_ptr.add(hdr_bytes.len()),
                req_bytes.len(),
            );
        }
        let rndis_total_bytes = (hdr_bytes.len() + req_bytes.len()) as u32;
        log::debug!(
            "netvsp: rndis buffer at GPA {:#x}, {} bytes",
            virt_to_phys(rndis_ptr),
            rndis_total_bytes
        );

        // Build the NVSP wrapper.
        let mut nvsp_frame = [0u8; NVSP_V61_MESSAGE_SIZE];
        let n = encode_message(
            msg_type::V1_SEND_RNDIS_PKT,
            &nvsp::Message1SendRndisPacket {
                channel_type: RMC_CONTROL,
                send_buffer_section_index: NETVSC_INVALID_INDEX,
                send_buffer_section_size: 0,
            },
            self.version_typed()?,
            &mut nvsp_frame,
        )
        .map_err(|_| Error::Parse {
            ty: None,
            reason: "encode SEND_RNDIS_PKT",
        })?;

        // Send GPA-direct with completion.
        let tid = self.alloc_transaction_id();
        let mut flags = PacketFlags::new();
        flags.set_request_completion(true);
        let rndis_gpa = virt_to_phys(rndis_ptr);
        let pfns = [rndis_gpa >> 12];
        let offset = (rndis_gpa & 0xFFF) as u32;
        if self.channel.state() != ChannelState::Open {
            return Err(Error::Rescinded);
        }
        let need_signal = self.send.write_gpa_direct(
            &pfns,
            offset,
            rndis_total_bytes,
            &nvsp_frame[..n],
            flags,
            tid,
        )?;
        if need_signal {
            self.channel.signal(ctx)?;
        }
        log::debug!("netvsp: RNDIS INITIALIZE sent (tid={:#x})", tid);

        // Wait for the two responses on the recv ring:
        // (a) VM_PKT_COMP with our tid → acks the NVSP send.
        // (b) VM_PKT_DATA_USING_XFER_PAGES → carries
        //     RNDIS_INITIALIZE_COMPLETE inside the recv buffer.
        // They can arrive in either order in principle; puppet
        // observed nvsp completion first, then xfer-page. We accept
        // both orders.
        let mut got_nvsp_comp = false;
        let mut got_rndis_response = false;
        let mut buf = [0u8; 512];
        for _ in 0..DEFAULT_MAX_POLLS {
            match self.recv.read(&mut buf) {
                Ok(pkt) => {
                    match pkt.descriptor.packet_type {
                        PacketType::VM_PKT_COMP if pkt.descriptor.transaction_id == tid => {
                            log::debug!("netvsp: got V1_SEND_RNDIS_PKT_COMPLETE (tid={:#x})", tid);
                            got_nvsp_comp = true;
                        }
                        PacketType::VM_PKT_DATA_USING_XFER_PAGES => {
                            // Parse: pkt.buf starts at ext_header,
                            // then payload. buf[..ext_header_len] is
                            // the transfer-page header + ranges.
                            let ext_len = pkt.ext_header_len;
                            let host_tid = pkt.descriptor.transaction_id;
                            let (xhdr, _) =
                                TransferPageHeader::read_from_prefix(&buf).map_err(|_| {
                                    Error::Parse {
                                        ty: None,
                                        reason: "parse TransferPageHeader",
                                    }
                                })?;
                            log::debug!(
                                "netvsp: xfer-page packet: set_id={:#x} range_count={} host_tid={:#x}",
                                xhdr.transfer_page_set_id,
                                xhdr.range_count,
                                host_tid
                            );
                            if xhdr.transfer_page_set_id != NETVSC_RECEIVE_BUFFER_ID {
                                log::warn!(
                                    "netvsp: xfer-page set_id={:#x} != NETVSC_RECEIVE_BUFFER_ID",
                                    xhdr.transfer_page_set_id
                                );
                                // Still ack — the host has already
                                // handed us the transfer-page range,
                                // and if we don't ack it stays
                                // reserved forever.
                                self.ack_xfer_page(ctx, host_tid)?;
                                continue;
                            }
                            if xhdr.range_count == 0 {
                                self.ack_xfer_page(ctx, host_tid)?;
                                continue;
                            }
                            // First range describes the RNDIS message.
                            let (range0, _) = TransferPageRange::read_from_prefix(&buf[8..])
                                .map_err(|_| Error::Parse {
                                    ty: None,
                                    reason: "parse TransferPageRange",
                                })?;
                            log::debug!(
                                "netvsp: xfer-page range0: offset={:#x} count={}",
                                range0.byte_offset,
                                range0.byte_count,
                            );
                            if (range0.byte_offset as usize + range0.byte_count as usize) > recv_len
                            {
                                return Err(Error::Parse {
                                    ty: None,
                                    reason: "xfer-page range out of recv buf",
                                });
                            }
                            // SAFETY: recv_base + byte_offset..
                            // +byte_count is within the recv buffer
                            // we own; single-threaded UEFI.
                            #[expect(unsafe_code, reason = "read RNDIS response from recv buffer")]
                            let rndis_msg = unsafe {
                                from_raw_parts(
                                    recv_base.add(range0.byte_offset as usize),
                                    range0.byte_count as usize,
                                )
                            };
                            let (rhdr, rest) = rndisprot::MessageHeader::read_from_prefix(
                                rndis_msg,
                            )
                            .map_err(|_| Error::Parse {
                                ty: None,
                                reason: "parse rndisprot::MessageHeader",
                            })?;
                            log::debug!(
                                "netvsp: RNDIS response type={:#x} len={}",
                                rhdr.message_type,
                                rhdr.message_length,
                            );
                            if rhdr.message_type != rndisprot::MESSAGE_TYPE_INITIALIZE_CMPLT {
                                return Err(Error::Parse {
                                    ty: None,
                                    reason: "expected RNDIS_INITIALIZE_CMPLT",
                                });
                            }
                            let (comp, _) = rndisprot::InitializeComplete::read_from_prefix(rest)
                                .map_err(|_| Error::Parse {
                                ty: None,
                                reason: "parse rndisprot::InitializeComplete",
                            })?;
                            log::debug!(
                                "netvsp: RNDIS init complete status={:#x} request_id={:#x} \
                                 major={} minor={} device_flags={:#x} medium={} \
                                 max_packets={} max_transfer={}",
                                comp.status,
                                comp.request_id,
                                comp.major_version,
                                comp.minor_version,
                                comp.device_flags,
                                comp.medium,
                                comp.max_packets_per_message,
                                comp.max_transfer_size,
                            );
                            if comp.status != rndisprot::STATUS_SUCCESS {
                                return Err(Error::Parse {
                                    ty: None,
                                    reason: "RNDIS init status != SUCCESS",
                                });
                            }
                            if comp.request_id != request_id {
                                return Err(Error::Parse {
                                    ty: None,
                                    reason: "RNDIS response request_id mismatch",
                                });
                            }

                            // Send VM_PKT_COMP back so the host can
                            // free its transfer pages. Payload =
                            // nvsp::Message1SendRndisPacketComplete { SUCCESS }
                            // per puppet's convention.
                            let mut comp_frame = [0u8; NVSP_V61_MESSAGE_SIZE];
                            let m = encode_message(
                                msg_type::V1_SEND_RNDIS_PKT_COMPLETE,
                                &nvsp::Message1SendRndisPacketComplete {
                                    status: Status::SUCCESS,
                                },
                                self.version_typed()?,
                                &mut comp_frame,
                            )
                            .map_err(|_| Error::Parse {
                                ty: None,
                                reason: "encode V1_SEND_RNDIS_PKT_COMPLETE",
                            })?;
                            let need_signal =
                                self.send.write_completion(&comp_frame[..m], host_tid)?;
                            if need_signal {
                                self.channel.signal(ctx)?;
                            }
                            log::debug!(
                                "netvsp: xfer-page COMP sent back (host_tid={:#x})",
                                host_tid
                            );
                            got_rndis_response = true;
                            let _ = ext_len;
                        }
                        _ => {
                            log::debug!(
                                "netvsp: unexpected packet during rndis_init: type={:#x} tid={:#x}",
                                pkt.descriptor.packet_type.0,
                                pkt.descriptor.transaction_id,
                            );
                        }
                    }
                    if got_nvsp_comp && got_rndis_response {
                        return Ok(());
                    }
                }
                Err(Error::RingEmpty) => {
                    spin_loop();
                }
                Err(e) => return Err(e),
            }
        }
        log::warn!(
            "netvsp: rndis_init timed out (got_nvsp_comp={} got_rndis_response={})",
            got_nvsp_comp,
            got_rndis_response
        );
        Err(Error::Timeout)
    }

    /// Set the NDIS packet filter to accept common frame types
    /// (broadcast, all multicast, directed unicast to our MAC).
    ///
    /// **Must be called before the host will deliver any Ethernet
    /// frames.** Until this succeeds, NDIS's filter is 0 → every
    /// frame is silently dropped by the vSwitch/netvsp pipeline
    /// **on the receive side** (send still works). Matches Linux's
    /// `rndis_filter_open`.
    ///
    /// Sequence:
    /// 1. Allocate a page-aligned buffer, write `rndisprot::MessageHeader
    ///    + rndisprot::SetRequest + [filter u32]`.
    /// 2. Send via GPA-direct wrapped in `V1_SEND_RNDIS_PKT(RMC_CONTROL)`.
    /// 3. Wait for `V1_SEND_RNDIS_PKT_COMPLETE` (NVSP-level ack).
    /// 4. Wait for `RNDIS_SET_CMPLT` on the xfer-page path.
    /// 5. Send `VM_PKT_COMP` back to release the transfer pages.
    ///
    /// Requires [`Self::rndis_init`] to have already succeeded.
    pub fn set_packet_filter<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        &mut self,
        ctx: &mut C,
        filter: u32,
    ) -> Result<()> {
        let recv_buf = self.recv_buf.as_ref().ok_or(Error::Parse {
            ty: None,
            reason: "set_packet_filter requires recv buffer",
        })?;
        let recv_base = recv_buf.ptr;
        let recv_len = recv_buf.len;

        let rndis_layout = Layout::from_size_align(4096, 4096).map_err(|_| Error::Parse {
            ty: None,
            reason: "set_pkt_filter buffer layout",
        })?;
        // SAFETY: page-sized page-aligned request.
        #[expect(unsafe_code, reason = "page-aligned RNDIS SET buffer")]
        let rndis_ptr = unsafe { alloc_zeroed(rndis_layout) };
        if rndis_ptr.is_null() {
            return Err(Error::Parse {
                ty: None,
                reason: "set_pkt_filter buffer alloc",
            });
        }

        let hdr_size = size_of::<rndisprot::MessageHeader>();
        let req_size = size_of::<rndisprot::SetRequest>();
        let info_size = size_of::<u32>();
        let total_len = (hdr_size + req_size + info_size) as u32;

        let request_id: u32 = 2;
        let rndis_hdr = rndisprot::MessageHeader {
            message_type: rndisprot::MESSAGE_TYPE_SET_MSG,
            message_length: total_len,
        };
        let set_req = rndisprot::SetRequest {
            request_id,
            oid: rndisprot::Oid::OID_GEN_CURRENT_PACKET_FILTER,
            information_buffer_length: info_size as u32,
            information_buffer_offset: req_size as u32,
            device_vc_handle: 0,
        };

        // SAFETY: rndis_ptr is a valid 4 KiB allocation; total_len
        // = 8 + 20 + 4 = 32 << 4096.
        #[expect(unsafe_code, reason = "copy RNDIS bytes into own buffer")]
        unsafe {
            copy_nonoverlapping(rndis_hdr.as_bytes().as_ptr(), rndis_ptr, hdr_size);
            copy_nonoverlapping(
                set_req.as_bytes().as_ptr(),
                rndis_ptr.add(hdr_size),
                req_size,
            );
            copy_nonoverlapping(
                filter.to_le_bytes().as_ptr(),
                rndis_ptr.add(hdr_size + req_size),
                info_size,
            );
        }

        // NVSP wrapper.
        let mut nvsp_frame = [0u8; NVSP_V61_MESSAGE_SIZE];
        let n = encode_message(
            msg_type::V1_SEND_RNDIS_PKT,
            &nvsp::Message1SendRndisPacket {
                channel_type: RMC_CONTROL,
                send_buffer_section_index: NETVSC_INVALID_INDEX,
                send_buffer_section_size: 0,
            },
            self.version_typed()?,
            &mut nvsp_frame,
        )
        .map_err(|_| Error::Parse {
            ty: None,
            reason: "encode SEND_RNDIS_PKT (set)",
        })?;

        // GPA-direct send with completion.
        if self.channel.state() != ChannelState::Open {
            return Err(Error::Rescinded);
        }
        let tid = self.alloc_transaction_id();
        let mut flags = PacketFlags::new();
        flags.set_request_completion(true);
        let rndis_gpa = virt_to_phys(rndis_ptr);
        let pfns = [rndis_gpa >> 12];
        let offset = (rndis_gpa & 0xFFF) as u32;
        let need_signal =
            self.send
                .write_gpa_direct(&pfns, offset, total_len, &nvsp_frame[..n], flags, tid)?;
        if need_signal {
            self.channel.signal(ctx)?;
        }
        log::debug!(
            "netvsp: RNDIS SET packet_filter={:#x} sent (tid={:#x})",
            filter,
            tid
        );

        // Wait for both the NVSP send-completion AND the RNDIS SET
        // response. Same pattern as rndis_init.
        let mut got_nvsp_comp = false;
        let mut got_rndis_response = false;
        let mut buf = [0u8; 512];
        for _ in 0..DEFAULT_MAX_POLLS {
            match self.recv.read(&mut buf) {
                Ok(pkt) => {
                    match pkt.descriptor.packet_type {
                        PacketType::VM_PKT_COMP if pkt.descriptor.transaction_id == tid => {
                            log::debug!("netvsp: RNDIS SET nvsp-comp received");
                            got_nvsp_comp = true;
                        }
                        PacketType::VM_PKT_DATA_USING_XFER_PAGES => {
                            let host_tid = pkt.descriptor.transaction_id;
                            let (xhdr, _) =
                                TransferPageHeader::read_from_prefix(&buf).map_err(|_| {
                                    Error::Parse {
                                        ty: None,
                                        reason: "parse TransferPageHeader",
                                    }
                                })?;
                            if xhdr.range_count == 0 {
                                self.ack_xfer_page(ctx, host_tid)?;
                                continue;
                            }
                            let (r0, _) =
                                TransferPageRange::read_from_prefix(&buf[8..]).map_err(|_| {
                                    Error::Parse {
                                        ty: None,
                                        reason: "parse TransferPageRange",
                                    }
                                })?;
                            if (r0.byte_offset as usize + r0.byte_count as usize) > recv_len {
                                self.ack_xfer_page(ctx, host_tid)?;
                                continue;
                            }
                            // SAFETY: bounds-checked; single-threaded.
                            #[expect(unsafe_code, reason = "read RNDIS SET response from recv")]
                            let rndis_msg = unsafe {
                                from_raw_parts(
                                    recv_base.add(r0.byte_offset as usize),
                                    r0.byte_count as usize,
                                )
                            };
                            let (rhdr, rest) = rndisprot::MessageHeader::read_from_prefix(
                                rndis_msg,
                            )
                            .map_err(|_| Error::Parse {
                                ty: None,
                                reason: "parse rndisprot::MessageHeader",
                            })?;
                            if rhdr.message_type == rndisprot::MESSAGE_TYPE_SET_CMPLT {
                                let (sc, _) = rndisprot::SetComplete::read_from_prefix(rest)
                                    .map_err(|_| Error::Parse {
                                        ty: None,
                                        reason: "parse rndisprot::SetComplete",
                                    })?;
                                log::debug!(
                                    "netvsp: RNDIS SET_CMPLT request_id={:#x} status={:#x}",
                                    sc.request_id,
                                    sc.status,
                                );
                                if sc.status != rndisprot::STATUS_SUCCESS {
                                    return Err(Error::Parse {
                                        ty: None,
                                        reason: "RNDIS SET status != SUCCESS",
                                    });
                                }
                                if sc.request_id != request_id {
                                    return Err(Error::Parse {
                                        ty: None,
                                        reason: "RNDIS SET request_id mismatch",
                                    });
                                }
                                got_rndis_response = true;
                            }
                            // Regardless of whether it was our SET
                            // response or unrelated traffic, ack the
                            // xfer-page so the host can reap it.
                            let mut cf = [0u8; NVSP_V61_MESSAGE_SIZE];
                            let m = encode_message(
                                msg_type::V1_SEND_RNDIS_PKT_COMPLETE,
                                &nvsp::Message1SendRndisPacketComplete {
                                    status: Status::SUCCESS,
                                },
                                self.version_typed()?,
                                &mut cf,
                            )
                            .map_err(|_| Error::Parse {
                                ty: None,
                                reason: "encode SET-ack",
                            })?;
                            let need_signal = self.send.write_completion(&cf[..m], host_tid)?;
                            if need_signal {
                                self.channel.signal(ctx)?;
                            }
                        }
                        _ => {
                            log::debug!(
                                "netvsp: unexpected packet during SET: type={:#x} tid={:#x}",
                                pkt.descriptor.packet_type.0,
                                pkt.descriptor.transaction_id,
                            );
                        }
                    }
                    if got_nvsp_comp && got_rndis_response {
                        return Ok(());
                    }
                }
                Err(Error::RingEmpty) => spin_loop(),
                Err(e) => return Err(e),
            }
        }
        log::warn!(
            "netvsp: set_packet_filter timed out (nvsp={} rndis={})",
            got_nvsp_comp,
            got_rndis_response
        );
        Err(Error::Timeout)
    }

    /// Send a raw Ethernet frame via RNDIS `PACKET_MSG` wrapped in
    /// `nvsp::Message1SendRndisPacket(RMC_DATA)`, using a GPA-direct
    /// external buffer for the RNDIS message.
    ///
    /// This is the TX equivalent of what puppet's `send_eth_packet`
    /// does. `wait_for_completion` controls whether we spin waiting
    /// for the paired `V1_SEND_RNDIS_PKT_COMPLETE` (round-trip
    /// send) or return as soon as the packet is signalled (bulk
    /// stress). In **both** modes the RNDIS buffer is registered
    /// with an internal `pending_tx` tracker and reclaimed when its
    /// completion is later observed by [`Self::drain_inbound`],
    /// [`Self::flush_tx`], or a subsequent `send_ethernet` call.
    /// Callers doing bursts of fire-and-forget sends must
    /// periodically call `drain_inbound` (or `flush_tx` at the end)
    /// to bound the outstanding heap footprint and to keep the
    /// recv ring from filling up.
    ///
    /// Requires [`Self::rndis_init`] to have succeeded (the host
    /// won't accept data packets before RNDIS init).
    ///
    /// # Wire layout
    ///
    /// The RNDIS message written to the buffer:
    /// ```text
    /// rndisprot::MessageHeader { PACKET_MSG, message_length }
    /// rndisprot::Packet { data_offset = size_of::<Packet>, data_length = frame.len() }
    /// [frame bytes]
    /// ```
    /// `data_offset` is measured from the start of the `rndisprot::Packet`
    /// struct (openvmm convention). Note that per_packet_info /
    /// oob_data all zero for a plain unadorned frame.
    ///
    /// The NVSP wrapper:
    /// ```text
    /// nvsp::Message1SendRndisPacket {
    ///   channel_type = RMC_DATA (0),
    ///   send_buffer_section_index = NETVSC_INVALID_INDEX,
    ///   send_buffer_section_size = 0,
    /// }
    /// ```
    pub fn send_ethernet<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        &mut self,
        ctx: &mut C,
        frame: &[u8],
        wait_for_completion: bool,
    ) -> Result<()> {
        if frame.is_empty() || frame.len() > 4096 - 64 {
            return Err(Error::Parse {
                ty: None,
                reason: "ethernet frame size out of range",
            });
        }
        if self.recv_buf.is_none() {
            return Err(Error::Parse {
                ty: None,
                reason: "send_ethernet requires establish_recv_buffer first",
            });
        }

        // Allocate a page-aligned buffer. Layout inside:
        //   [0..8)   rndisprot::MessageHeader
        //   [8..44)  rndisprot::Packet (36 bytes)
        //   [44..)   Ethernet frame
        let rndis_layout = Layout::from_size_align(4096, 4096).map_err(|_| Error::Parse {
            ty: None,
            reason: "rndis buffer layout",
        })?;
        // SAFETY: validated single-page layout, single-threaded UEFI.
        #[expect(unsafe_code, reason = "page-aligned RNDIS message allocation")]
        let rndis_ptr = unsafe { alloc_zeroed(rndis_layout) };
        if rndis_ptr.is_null() {
            return Err(Error::Parse {
                ty: None,
                reason: "rndis buffer alloc failed",
            });
        }
        let hdr_size = size_of::<rndisprot::MessageHeader>();
        let pkt_size = size_of::<rndisprot::Packet>();
        let total_len = (hdr_size + pkt_size + frame.len()) as u32;

        let rndis_hdr = rndisprot::MessageHeader {
            message_type: rndisprot::MESSAGE_TYPE_PACKET_MSG,
            message_length: total_len,
        };
        // data_offset is measured from the START of rndisprot::Packet,
        // not from the start of the whole RNDIS message. So it's
        // just pkt_size (the frame sits immediately after the Packet
        // struct).
        let rndis_pkt = rndisprot::Packet {
            data_offset: pkt_size as u32,
            data_length: frame.len() as u32,
            oob_data_offset: 0,
            oob_data_length: 0,
            num_oob_data_elements: 0,
            per_packet_info_offset: 0,
            per_packet_info_length: 0,
            vc_handle: 0,
            reserved: 0,
        };

        // SAFETY: rndis_ptr is a valid 4 KiB allocation, and
        // hdr_size + pkt_size + frame.len() <= 4096 by the input
        // check above.
        #[expect(unsafe_code, reason = "copy RNDIS message parts into own buffer")]
        unsafe {
            copy_nonoverlapping(rndis_hdr.as_bytes().as_ptr(), rndis_ptr, hdr_size);
            copy_nonoverlapping(
                rndis_pkt.as_bytes().as_ptr(),
                rndis_ptr.add(hdr_size),
                pkt_size,
            );
            copy_nonoverlapping(
                frame.as_ptr(),
                rndis_ptr.add(hdr_size + pkt_size),
                frame.len(),
            );
        }

        // Build the NVSP wrapper.
        let mut nvsp_frame = [0u8; NVSP_V61_MESSAGE_SIZE];
        let n = encode_message(
            msg_type::V1_SEND_RNDIS_PKT,
            &nvsp::Message1SendRndisPacket {
                channel_type: RMC_DATA,
                send_buffer_section_index: NETVSC_INVALID_INDEX,
                send_buffer_section_size: 0,
            },
            self.version_typed()?,
            &mut nvsp_frame,
        )
        .map_err(|_| Error::Parse {
            ty: None,
            reason: "encode SEND_RNDIS_PKT (data)",
        })?;

        // Post via GPA-direct. Always request completion so we can
        // track and eventually free `rndis_ptr` — leaking a page per
        // frame is not viable under sustained load. Fire-and-forget
        // callers can pick up the completion asynchronously via
        // `drain_inbound` / `flush_tx`.
        if self.channel.state() != ChannelState::Open {
            // SAFETY: we own `rndis_ptr` and no aliasing has occurred.
            #[expect(unsafe_code, reason = "reclaim allocation on early exit")]
            unsafe {
                dealloc(rndis_ptr, rndis_layout);
            }
            return Err(Error::Rescinded);
        }
        // Cap the outstanding-TX queue: opportunistically drain, and
        // if still full, force-reclaim the oldest buffers so the send
        // path stays alive instead of wedging at `RingFull`.
        if self.pending_tx.len() >= PENDING_TX_MAX {
            let _ = self.drain_inbound(ctx, PENDING_TX_MAX, |_| {});
            if self.pending_tx.len() >= PENDING_TX_MAX {
                // Draining reaped no completions — under fuzzing these
                // are sends the host silently dropped, so they will
                // never complete and would leak forever. Force-reclaim
                // the oldest buffers to make room.
                self.reclaim_oldest_tx(PENDING_TX_RECLAIM);
            }
        }
        let tid = self.alloc_transaction_id();
        let mut flags = PacketFlags::new();
        flags.set_request_completion(true);
        let rndis_gpa = virt_to_phys(rndis_ptr);
        let pfns = [rndis_gpa >> 12];
        let offset = (rndis_gpa & 0xFFF) as u32;
        let need_signal =
            match self.post_gpa_direct(ctx, &pfns, offset, total_len, &nvsp_frame[..n], flags, tid)
            {
                Ok(s) => s,
                Err(e) => {
                    // Not yet tracked in `pending_tx`; free the buffer.
                    // SAFETY: we own `rndis_ptr` and no aliasing has occurred.
                    #[expect(unsafe_code, reason = "reclaim allocation on send failure")]
                    unsafe {
                        dealloc(rndis_ptr, rndis_layout);
                    }
                    return Err(e);
                }
            };
        if need_signal {
            self.channel.signal(ctx)?;
        }
        // Register the buffer for reclaim BEFORE we might block on
        // the recv ring: any code path from here on that returns
        // early must not touch `rndis_ptr` (the tracker owns it now).
        self.pending_tx.push_back(PendingTx {
            tid,
            ptr: rndis_ptr,
            layout: rndis_layout,
        });

        if !wait_for_completion {
            // Fire-and-forget: opportunistically drain any pending
            // recv-side traffic so the recv ring doesn't fill up
            // (which would eventually starve the host's ability to
            // deliver our TX completions). We do a bounded non-
            // blocking pass — the caller is responsible for periodic
            // `drain_inbound` / `flush_tx` under sustained bursts.
            let _ = self.drain_inbound(ctx, 0, |_| {});
            return Ok(());
        }

        // Wait for V1_SEND_RNDIS_PKT_COMPLETE (matching tid). Use a
        // 4 KiB scratch buffer to accommodate large xfer-page
        // packets that may arrive interleaved with our completion —
        // a 512-byte buffer would trip `RingEmpty`'s "recv buffer
        // smaller than packet payload" error and wedge the ring
        // (the packet stays at head, no future read makes progress).
        let mut buf = [0u8; 4096];
        for _ in 0..DEFAULT_MAX_POLLS {
            match self.recv.read(&mut buf) {
                Ok(pkt) => {
                    match pkt.descriptor.packet_type {
                        PacketType::VM_PKT_COMP if pkt.descriptor.transaction_id == tid => {
                            // Our completion — parse status, free our
                            // buffer, return.
                            let (_hdr, body) =
                                parse_header(pkt.payload).map_err(|_| Error::Parse {
                                    ty: None,
                                    reason: "parse SEND_RNDIS_PKT_COMPLETE hdr",
                                })?;
                            let (comp, _) =
                                nvsp::Message1SendRndisPacketComplete::read_from_prefix(body)
                                    .map_err(|_| Error::Parse {
                                        ty: None,
                                        reason: "parse SEND_RNDIS_PKT_COMPLETE body",
                                    })?;
                            let ok = comp.status == Status::SUCCESS;
                            let _ = self.free_completed_tx(tid);
                            if !ok {
                                return Err(Error::Parse {
                                    ty: None,
                                    reason: "SEND_RNDIS_PKT_COMPLETE non-success status",
                                });
                            }
                            return Ok(());
                        }
                        PacketType::VM_PKT_COMP => {
                            // A completion for a previous fire-and-
                            // forget — reap the associated buffer.
                            let _ = self.free_completed_tx(pkt.descriptor.transaction_id);
                        }
                        PacketType::VM_PKT_DATA_USING_XFER_PAGES => {
                            // An inbound Ethernet delivery. We MUST
                            // ack it so the host can reclaim the
                            // transfer-page range; skipping the ack
                            // eventually stalls the host RX path and
                            // (transitively) our TX completions.
                            let host_tid = pkt.descriptor.transaction_id;
                            self.ack_xfer_page(ctx, host_tid)?;
                            // Note: the frame bytes are dropped here.
                            // Callers that want to receive frames
                            // should use `drain_inbound` on the pre-
                            // send / post-flush path.
                        }
                        _ => {
                            log::debug!(
                                "netvsp: skipping packet during send: type={:#x} tid={:#x}",
                                pkt.descriptor.packet_type.0,
                                pkt.descriptor.transaction_id,
                            );
                        }
                    }
                }
                Err(Error::RingEmpty) => spin_loop(),
                Err(e) => return Err(e),
            }
        }
        Err(Error::Timeout)
    }

    /// Drain any pending inbound Ethernet frames (host → guest) and
    /// invoke `on_frame` with each one's bytes.
    ///
    /// The host delivers frames as `VM_PKT_DATA_USING_XFER_PAGES`
    /// packets referencing recv-buffer sections. We also send back
    /// `VM_PKT_COMP` for each so the host can free its transfer
    /// pages.
    ///
    /// Returns the number of frames drained. `max_polls` bounds the
    /// spin — 0 means "one non-blocking pass; return whatever's
    /// currently on the ring".
    pub fn drain_inbound<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        &mut self,
        ctx: &mut C,
        max_polls: usize,
        mut on_frame: impl FnMut(&[u8]),
    ) -> Result<usize> {
        let recv_buf = self.recv_buf.as_ref().ok_or(Error::Parse {
            ty: None,
            reason: "drain_inbound requires establish_recv_buffer first",
        })?;
        let recv_base = recv_buf.ptr;
        let recv_len = recv_buf.len;
        let mut count = 0usize;
        let mut buf = [0u8; 4096];
        let iters = if max_polls == 0 { 1 } else { max_polls };
        for _ in 0..iters {
            match self.recv.read(&mut buf) {
                Ok(pkt) => match pkt.descriptor.packet_type {
                    PacketType::VM_PKT_DATA_USING_XFER_PAGES => {
                        let host_tid = pkt.descriptor.transaction_id;
                        let (xhdr, _) =
                            TransferPageHeader::read_from_prefix(&buf).map_err(|_| {
                                Error::Parse {
                                    ty: None,
                                    reason: "parse TransferPageHeader",
                                }
                            })?;
                        for i in 0..xhdr.range_count as usize {
                            let range_off = 8 + i * 8;
                            let (r, _) = TransferPageRange::read_from_prefix(&buf[range_off..])
                                .map_err(|_| Error::Parse {
                                    ty: None,
                                    reason: "parse TransferPageRange",
                                })?;
                            if (r.byte_offset as usize + r.byte_count as usize) > recv_len {
                                continue;
                            }
                            // SAFETY: bounds-checked above; single-threaded.
                            #[expect(unsafe_code, reason = "read from recv buffer")]
                            let msg = unsafe {
                                from_raw_parts(
                                    recv_base.add(r.byte_offset as usize),
                                    r.byte_count as usize,
                                )
                            };
                            let (rhdr, rest) = rndisprot::MessageHeader::read_from_prefix(msg)
                                .map_err(|_| Error::Parse {
                                    ty: None,
                                    reason: "parse rndisprot::MessageHeader",
                                })?;
                            if rhdr.message_type == rndisprot::MESSAGE_TYPE_PACKET_MSG {
                                let (rp, _) =
                                    rndisprot::Packet::read_from_prefix(rest).map_err(|_| {
                                        Error::Parse {
                                            ty: None,
                                            reason: "parse rndisprot::Packet",
                                        }
                                    })?;
                                let frame_off = rp.data_offset as usize;
                                let frame_len = rp.data_length as usize;
                                if frame_off + frame_len <= rest.len() {
                                    on_frame(&rest[frame_off..frame_off + frame_len]);
                                    count += 1;
                                }
                            } else {
                                log::debug!(
                                    "netvsp: drain saw non-packet RNDIS type {:#x}",
                                    rhdr.message_type
                                );
                            }
                        }
                        // Send VM_PKT_COMP so the host can reap the
                        // transfer pages.
                        let mut comp_frame = [0u8; NVSP_V61_MESSAGE_SIZE];
                        let m = encode_message(
                            msg_type::V1_SEND_RNDIS_PKT_COMPLETE,
                            &nvsp::Message1SendRndisPacketComplete {
                                status: Status::SUCCESS,
                            },
                            self.version_typed()?,
                            &mut comp_frame,
                        )
                        .map_err(|_| Error::Parse {
                            ty: None,
                            reason: "encode V1_SEND_RNDIS_PKT_COMPLETE",
                        })?;
                        let need_signal = self.send.write_completion(&comp_frame[..m], host_tid)?;
                        if need_signal {
                            self.channel.signal(ctx)?;
                        }
                    }
                    PacketType::VM_PKT_COMP => {
                        // Might be a completion for a fire-and-forget
                        // (or previously-abandoned) `send_ethernet`
                        // TX buffer we're still holding — try to
                        // reclaim.
                        let tid = pkt.descriptor.transaction_id;
                        if !self.free_completed_tx(tid) {
                            log::debug!("netvsp: drain saw stale VM_PKT_COMP tid={:#x}", tid,);
                        }
                    }
                    _ => {
                        log::debug!(
                            "netvsp: drain saw unexpected packet type {:#x}",
                            pkt.descriptor.packet_type.0
                        );
                    }
                },
                Err(Error::RingEmpty) => {
                    if max_polls == 0 {
                        break;
                    }
                    spin_loop();
                }
                Err(e) => return Err(e),
            }
        }
        Ok(count)
    }

    /// Send a raw, caller-provided NVSP message frame as a
    /// `VM_PKT_DATA_INBAND` packet on the netvsp channel.
    ///
    /// This is the low-level primitive backing the fuzzer's
    /// `send_nvsp` call: the bytes in `frame` are transmitted
    /// verbatim, so a fuzzer can drive arbitrary (well-formed or
    /// malformed) NVSP messages at the host. When `completion` is set
    /// the completion-requested flag is set and we spin for the
    /// matching `VM_PKT_COMP`, discarding its payload; otherwise the
    /// packet is fire-and-forget.
    pub fn send_nvsp_raw<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        &mut self,
        ctx: &mut C,
        frame: &[u8],
        completion: bool,
    ) -> Result<()> {
        if frame.is_empty() {
            return Err(Error::Parse {
                ty: None,
                reason: "send_nvsp_raw: empty frame",
            });
        }
        if completion {
            let _ = self.send_and_await(ctx, frame, FUZZ_SEND_MAX_POLLS)?;
            Ok(())
        } else {
            self.send_no_completion(ctx, frame)
        }
    }

    /// Send a raw, caller-provided RNDIS message wrapped in an NVSP
    /// `V1_SEND_RNDIS_PKT`, delivered to the host via GPA-direct.
    ///
    /// This backs the fuzzer's `send_rndis` call: `rndis` is the
    /// complete RNDIS message (starting with its
    /// `rndisprot::MessageHeader`), copied verbatim into a fresh
    /// page-aligned buffer whose GPA range is handed to the host.
    /// `channel_type` selects [`RMC_DATA`] vs [`RMC_CONTROL`]. When
    /// `completion` is set we wait for the
    /// `V1_SEND_RNDIS_PKT_COMPLETE` and free the buffer; otherwise
    /// the buffer is tracked in `pending_tx` and reclaimed lazily by
    /// a later drain.
    pub fn send_rndis_raw<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        &mut self,
        ctx: &mut C,
        channel_type: u32,
        rndis: &[u8],
        completion: bool,
    ) -> Result<()> {
        /// Max RNDIS payload the fuzzer may hand us. Bounds the
        /// per-call GPA-direct allocation and stays within the
        /// ring's 32-PFN GPA-direct limit.
        const MAX_RNDIS_LEN: usize = 16 * 4096;
        if rndis.is_empty() || rndis.len() > MAX_RNDIS_LEN {
            return Err(Error::Parse {
                ty: None,
                reason: "send_rndis_raw: payload size out of range",
            });
        }
        if self.channel.state() != ChannelState::Open {
            return Err(Error::Rescinded);
        }

        // Page-aligned buffer sized to hold the whole message.
        let pages = rndis.len().div_ceil(4096);
        let alloc_size = pages * 4096;
        let rndis_layout = Layout::from_size_align(alloc_size, 4096).map_err(|_| Error::Parse {
            ty: None,
            reason: "send_rndis_raw: buffer layout",
        })?;
        // SAFETY: validated non-zero page-aligned layout, single-
        // threaded UEFI.
        #[expect(unsafe_code, reason = "page-aligned RNDIS message allocation")]
        let rndis_ptr = unsafe { alloc_zeroed(rndis_layout) };
        if rndis_ptr.is_null() {
            return Err(Error::Parse {
                ty: None,
                reason: "send_rndis_raw: buffer alloc failed",
            });
        }
        // SAFETY: `rndis_ptr` is a valid `alloc_size >= rndis.len()`
        // allocation.
        #[expect(unsafe_code, reason = "copy caller RNDIS bytes into own buffer")]
        unsafe {
            copy_nonoverlapping(rndis.as_ptr(), rndis_ptr, rndis.len());
        }

        // Build the NVSP wrapper.
        let nvsp_res = self.version_typed().and_then(|version| {
            let mut nvsp_frame = [0u8; NVSP_V61_MESSAGE_SIZE];
            let n = encode_message(
                msg_type::V1_SEND_RNDIS_PKT,
                &nvsp::Message1SendRndisPacket {
                    channel_type,
                    send_buffer_section_index: NETVSC_INVALID_INDEX,
                    send_buffer_section_size: 0,
                },
                version,
                &mut nvsp_frame,
            )
            .map_err(|_| Error::Parse {
                ty: None,
                reason: "encode SEND_RNDIS_PKT (raw)",
            })?;
            Ok((nvsp_frame, n))
        });
        let (nvsp_frame, n) = match nvsp_res {
            Ok(v) => v,
            Err(e) => {
                // SAFETY: we own `rndis_ptr`, no aliasing occurred.
                #[expect(unsafe_code, reason = "reclaim allocation on early exit")]
                unsafe {
                    dealloc(rndis_ptr, rndis_layout);
                }
                return Err(e);
            }
        };

        // Cap the outstanding-TX queue like `send_ethernet`.
        if self.pending_tx.len() >= PENDING_TX_MAX {
            let _ = self.drain_inbound(ctx, PENDING_TX_MAX, |_| {});
            if self.pending_tx.len() >= PENDING_TX_MAX {
                // Draining reaped no completions — under fuzzing these
                // are sends the host silently dropped, so they will
                // never complete and would leak forever. Force-reclaim
                // the oldest buffers to keep the send path alive.
                self.reclaim_oldest_tx(PENDING_TX_RECLAIM);
            }
        }

        // Build the PFN list per-page (robust to non-contiguous
        // physical backing), then post via GPA-direct. Always request
        // completion so we can track and eventually free the buffer.
        let mut pfns = Vec::with_capacity(pages);
        for i in 0..pages {
            // SAFETY: offset stays within the `alloc_size` allocation.
            #[expect(unsafe_code, reason = "per-page virt->phys translation")]
            let page_ptr = unsafe { rndis_ptr.add(i * 4096) };
            pfns.push(virt_to_phys(page_ptr) >> 12);
        }
        let tid = self.alloc_transaction_id();
        let mut flags = PacketFlags::new();
        flags.set_request_completion(true);
        let need_signal = match self.post_gpa_direct(
            ctx,
            &pfns,
            0,
            rndis.len() as u32,
            &nvsp_frame[..n],
            flags,
            tid,
        ) {
            Ok(s) => s,
            Err(e) => {
                // Not yet tracked in `pending_tx`; free the buffer.
                // SAFETY: we own `rndis_ptr`, no aliasing occurred.
                #[expect(unsafe_code, reason = "reclaim allocation on send failure")]
                unsafe {
                    dealloc(rndis_ptr, rndis_layout);
                }
                return Err(e);
            }
        };
        if need_signal {
            self.channel.signal(ctx)?;
        }
        // The tracker owns `rndis_ptr` from here on.
        self.pending_tx.push_back(PendingTx {
            tid,
            ptr: rndis_ptr,
            layout: rndis_layout,
        });

        if !completion {
            // Fire-and-forget: opportunistic non-blocking drain so
            // the recv ring doesn't starve host TX completions.
            let _ = self.drain_inbound(ctx, 0, |_| {});
            return Ok(());
        }

        // Wait for our V1_SEND_RNDIS_PKT_COMPLETE (matching tid),
        // reaping other completions and acking xfer-page arrivals.
        let mut buf = [0u8; 4096];
        for _ in 0..FUZZ_SEND_MAX_POLLS {
            match self.recv.read(&mut buf) {
                Ok(pkt) => match pkt.descriptor.packet_type {
                    PacketType::VM_PKT_COMP if pkt.descriptor.transaction_id == tid => {
                        let _ = self.free_completed_tx(tid);
                        return Ok(());
                    }
                    PacketType::VM_PKT_COMP => {
                        let _ = self.free_completed_tx(pkt.descriptor.transaction_id);
                    }
                    PacketType::VM_PKT_DATA_USING_XFER_PAGES => {
                        let host_tid = pkt.descriptor.transaction_id;
                        self.ack_xfer_page(ctx, host_tid)?;
                    }
                    _ => {}
                },
                Err(Error::RingEmpty) => spin_loop(),
                Err(e) => return Err(e),
            }
        }
        Err(Error::Timeout)
    }

    /// Revoke and re-establish the receive buffer, reusing the
    /// existing GPADL registration (no new allocation).
    ///
    /// Backs the fuzzer's `renew_buffer` call for the receive buffer.
    /// Sends `V1_REVOKE_RECV_BUF` (fire-and-forget per protocol) then
    /// re-sends `V1_SEND_RECV_BUF` with the same GPADL handle and
    /// awaits the completion, refreshing the cached section geometry.
    pub fn renew_recv_buffer<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        &mut self,
        ctx: &mut C,
    ) -> Result<()> {
        let gpadl_handle = match &self.recv_buf {
            Some(b) => b.gpadl.id(),
            None => {
                return Err(Error::Parse {
                    ty: None,
                    reason: "renew_recv_buffer requires establish_recv_buffer first",
                });
            }
        };

        let mut frame = [0u8; NVSP_V61_MESSAGE_SIZE];
        let n = encode_message(
            msg_type::V1_REVOKE_RECV_BUF,
            &nvsp::Message1RevokeReceiveBuffer {
                id: NETVSC_RECEIVE_BUFFER_ID,
            },
            self.version_typed()?,
            &mut frame,
        )
        .map_err(|_| Error::Parse {
            ty: None,
            reason: "encode REVOKE_RECV_BUF",
        })?;
        self.send_no_completion(ctx, &frame[..n])?;
        self.recv_section_size = 0;
        self.recv_section_count = 0;

        let n = encode_message(
            msg_type::V1_SEND_RECV_BUF,
            &nvsp::Message1SendReceiveBuffer {
                gpadl_handle,
                id: NETVSC_RECEIVE_BUFFER_ID,
                reserved: 0,
            },
            self.version_typed()?,
            &mut frame,
        )
        .map_err(|_| Error::Parse {
            ty: None,
            reason: "encode SEND_RECV_BUF (renew)",
        })?;
        let response = self.send_and_await(ctx, &frame[..n], FUZZ_SEND_MAX_POLLS)?;
        let (ty, body) = parse_header(&response).map_err(|_| Error::Parse {
            ty: None,
            reason: "parse SEND_RECV_BUF_COMPLETE header (renew)",
        })?;
        if ty != msg_type::V1_SEND_RECV_BUF_COMPLETE {
            return Err(Error::Parse {
                ty: None,
                reason: "expected SEND_RECV_BUF_COMPLETE (renew)",
            });
        }
        let (parsed, _) =
            nvsp::Message1SendReceiveBufferComplete::read_from_prefix(body).map_err(|_| {
                Error::Parse {
                    ty: None,
                    reason: "parse SEND_RECV_BUF_COMPLETE body (renew)",
                }
            })?;
        if parsed.status != Status::SUCCESS || parsed.num_sections != 1 {
            return Err(Error::Parse {
                ty: None,
                reason: "recv-buf renew non-success / bad num_sections",
            });
        }
        let sec = &parsed.sections[0];
        self.recv_section_size = sec.sub_allocation_size;
        self.recv_section_count = sec.num_sub_allocations;
        Ok(())
    }

    /// Revoke and re-establish the send buffer, reusing the existing
    /// GPADL registration (no new allocation).
    ///
    /// Backs the fuzzer's `renew_buffer` call for the send buffer.
    pub fn renew_send_buffer<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        &mut self,
        ctx: &mut C,
    ) -> Result<()> {
        let gpadl_handle = match &self.send_buf {
            Some(b) => b.gpadl.id(),
            None => {
                return Err(Error::Parse {
                    ty: None,
                    reason: "renew_send_buffer requires establish_send_buffer first",
                });
            }
        };

        let mut frame = [0u8; NVSP_V61_MESSAGE_SIZE];
        // The revoke body is a bare id+pad; reuse the recv-buf revoke
        // struct with the send-buffer id.
        let n = encode_message(
            msg_type::V1_REVOKE_SEND_BUF,
            &nvsp::Message1RevokeReceiveBuffer {
                id: NETVSC_SEND_BUFFER_ID,
            },
            self.version_typed()?,
            &mut frame,
        )
        .map_err(|_| Error::Parse {
            ty: None,
            reason: "encode REVOKE_SEND_BUF",
        })?;
        self.send_no_completion(ctx, &frame[..n])?;
        self.send_section_size = 0;
        self.send_section_count = 0;

        let n = encode_message(
            msg_type::V1_SEND_SEND_BUF,
            &nvsp::Message1SendSendBuffer {
                gpadl_handle,
                id: NETVSC_SEND_BUFFER_ID,
                reserved: 0,
            },
            self.version_typed()?,
            &mut frame,
        )
        .map_err(|_| Error::Parse {
            ty: None,
            reason: "encode SEND_SEND_BUF (renew)",
        })?;
        let response = self.send_and_await(ctx, &frame[..n], FUZZ_SEND_MAX_POLLS)?;
        let (ty, body) = parse_header(&response).map_err(|_| Error::Parse {
            ty: None,
            reason: "parse SEND_SEND_BUF_COMPLETE header (renew)",
        })?;
        if ty != msg_type::V1_SEND_SEND_BUF_COMPLETE {
            return Err(Error::Parse {
                ty: None,
                reason: "expected SEND_SEND_BUF_COMPLETE (renew)",
            });
        }
        let (parsed, _) =
            nvsp::Message1SendSendBufferComplete::read_from_prefix(body).map_err(|_| {
                Error::Parse {
                    ty: None,
                    reason: "parse SEND_SEND_BUF_COMPLETE body (renew)",
                }
            })?;
        if parsed.status != Status::SUCCESS || parsed.section_size == 0 {
            return Err(Error::Parse {
                ty: None,
                reason: "send-buf renew non-success / zero section_size",
            });
        }
        self.send_section_size = parsed.section_size;
        Ok(())
    }

    /// Recover the underlying channel for closing.
    pub fn into_channel(self) -> Channel {
        self.channel
    }

    /// Split into the channel (to be closed by the caller) and the
    /// freeable guest [`NetvspBacking`].
    ///
    /// The `send`/`recv` rings are dropped here, but they only hold
    /// interior pointers into the ring region — the region itself, the
    /// GPADL buffers, and any outstanding TX staging buffers all travel
    /// out in the returned [`NetvspBacking`] so the caller can
    /// [`NetvspBacking::free`] them *after* closing the channel (once
    /// the host is no longer touching the pages). This is what makes a
    /// close/reopen cycle non-leaking.
    pub fn into_parts(self) -> (Channel, NetvspBacking) {
        let Netvsp {
            channel,
            ring_base,
            ring_layout,
            recv_buf,
            send_buf,
            pending_tx,
            ..
        } = self;
        let backing = NetvspBacking {
            ring_base,
            ring_layout,
            bufs: [recv_buf, send_buf],
            pending: pending_tx,
        };
        (channel, backing)
    }

    // ---- internals ----

    /// Post an NVSP frame with the completion-requested flag and
    /// spin until we receive a matching `VM_PKT_COMP`. Returns the
    /// completion frame's payload bytes (owned copy — the ring's
    /// bytes are consumed by then).
    fn send_and_await<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        &mut self,
        ctx: &mut C,
        frame: &[u8],
        max_polls: usize,
    ) -> Result<Vec<u8>> {
        if self.channel.state() != ChannelState::Open {
            return Err(Error::Rescinded);
        }
        let tid = self.alloc_transaction_id();
        let mut flags = PacketFlags::new();
        flags.set_request_completion(true);
        let need_signal = self.send.write_inband(frame, flags, tid)?;
        if need_signal {
            self.channel.signal(ctx)?;
        }

        // Spin the recv ring waiting for a VM_PKT_COMP with matching tid.
        let mut recv_buf = [0u8; 512];
        for _ in 0..max_polls {
            match self.recv.read(&mut recv_buf) {
                Ok(pkt) => {
                    if pkt.descriptor.packet_type == PacketType::VM_PKT_COMP
                        && pkt.descriptor.transaction_id == tid
                    {
                        // Copy payload out and return.
                        return Ok(pkt.payload.to_vec());
                    }
                    // Non-matching packet — log and keep looking. In
                    // Phase 1 we don't expect any, but Phase 3 will
                    // see unsolicited xfer-page arrivals.
                    log::debug!(
                        "netvsp: unexpected packet type={:#x} tid={:#x} while awaiting {:#x}",
                        pkt.descriptor.packet_type.0,
                        pkt.descriptor.transaction_id,
                        tid,
                    );
                }
                Err(Error::RingEmpty) => {
                    spin_loop();
                }
                Err(e) => return Err(e),
            }
        }
        Err(Error::Timeout)
    }

    /// Post an NVSP frame **without** the completion flag. Used for
    /// `SEND_NDIS_CONFIG` and `SEND_NDIS_VERSION` which have no
    /// reply per protocol.
    fn send_no_completion<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        &mut self,
        ctx: &mut C,
        frame: &[u8],
    ) -> Result<()> {
        if self.channel.state() != ChannelState::Open {
            return Err(Error::Rescinded);
        }
        let need_signal = self.send.write_inband(frame, PacketFlags::new(), 0)?;
        if need_signal {
            self.channel.signal(ctx)?;
        }
        Ok(())
    }

    fn alloc_transaction_id(&mut self) -> u64 {
        let tid = self.next_transaction_id;
        self.next_transaction_id = self.next_transaction_id.wrapping_add(1);
        if self.next_transaction_id == 0 {
            self.next_transaction_id = 1; // skip 0 sentinel
        }
        tid
    }

    /// Encode a `V1_SEND_RNDIS_PKT_COMPLETE(SUCCESS)` on the send
    /// ring targeting `host_tid`. Emitted for every
    /// `VM_PKT_DATA_USING_XFER_PAGES` we consume from the recv ring
    /// so the host can reclaim its transfer pages; without this ack
    /// the host will eventually stall as its transfer-page pool
    /// drains.
    fn ack_xfer_page<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        &mut self,
        ctx: &mut C,
        host_tid: u64,
    ) -> Result<()> {
        let mut cf = [0u8; NVSP_V61_MESSAGE_SIZE];
        let m = encode_message(
            msg_type::V1_SEND_RNDIS_PKT_COMPLETE,
            &nvsp::Message1SendRndisPacketComplete {
                status: Status::SUCCESS,
            },
            self.version_typed()?,
            &mut cf,
        )
        .map_err(|_| Error::Parse {
            ty: None,
            reason: "encode xfer-page ack",
        })?;
        let need_signal = self.send.write_completion(&cf[..m], host_tid)?;
        if need_signal {
            self.channel.signal(ctx)?;
        }
        Ok(())
    }

    /// If `tid` matches a buffer in [`Self::pending_tx`], drop it
    /// from the queue and free the backing allocation. Returns
    /// `true` on a hit.
    fn free_completed_tx(&mut self, tid: u64) -> bool {
        if let Some(idx) = self.pending_tx.iter().position(|p| p.tid == tid) {
            let entry = self.pending_tx.remove(idx).unwrap();
            // SAFETY: `entry.ptr` was returned by
            // `alloc_zeroed(entry.layout)` in
            // `send_ethernet` and has not been referenced by any
            // other code since. The paired host completion means
            // the host is done with the pages.
            #[expect(unsafe_code, reason = "free completed TX buffer")]
            unsafe {
                dealloc(entry.ptr, entry.layout);
            }
            true
        } else {
            false
        }
    }

    /// Force-free the oldest `n` outstanding TX buffers *without*
    /// waiting for their host completion. Returns the number freed.
    ///
    /// Used only when [`Self::pending_tx`] is saturated at
    /// [`PENDING_TX_MAX`] and a synchronous drain reaped nothing:
    /// under fuzzing the host silently drops malformed sends, so those
    /// tracker entries never receive a completion and would otherwise
    /// leak forever, wedging every subsequent GPA-direct send at
    /// `RingFull`.
    ///
    /// Safety trade-off: this deallocates a buffer the host could, in
    /// principle, still be reading during a slow TX. In practice a
    /// buffer outstanding for `PENDING_TX_MAX` (512) later sends has
    /// long since been consumed by the host, so the reuse-after-free
    /// window is negligible for a fuzzing harness.
    fn reclaim_oldest_tx(&mut self, n: usize) -> usize {
        let mut freed = 0;
        for _ in 0..n {
            match self.pending_tx.pop_front() {
                Some(entry) => {
                    // SAFETY: `entry.ptr` was returned by
                    // `alloc_zeroed(entry.layout)` in the send path and
                    // is owned solely by the tracker.
                    #[expect(unsafe_code, reason = "force-free leaked TX buffer")]
                    unsafe {
                        dealloc(entry.ptr, entry.layout);
                    }
                    freed += 1;
                }
                None => break,
            }
        }
        freed
    }

    /// Post a GPA-direct TX packet, recovering from a transiently
    /// full outbound ring. Under sustained fuzzing we post sends
    /// faster than the host drains them; once our recv ring fills
    /// with unreaped completions the host stops draining our send
    /// ring, which then wedges at `RingFull` permanently. On
    /// `RingFull` we reap completions off the recv ring
    /// ([`Self::drain_inbound`] frees recv-ring space *without*
    /// needing send-ring space) to relieve the host's backpressure,
    /// then retry. Returns the `need_signal` flag from the successful
    /// write, or [`Error::RingFull`] if the ring stays full after
    /// `RING_FULL_RETRIES` drain-and-retry passes.
    fn post_gpa_direct<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        &mut self,
        ctx: &mut C,
        pfns: &[u64],
        offset: u32,
        len: u32,
        nvsp_frame: &[u8],
        flags: PacketFlags,
        tid: u64,
    ) -> Result<bool> {
        const RING_FULL_RETRIES: usize = 8;
        for _ in 0..RING_FULL_RETRIES {
            match self
                .send
                .write_gpa_direct(pfns, offset, len, nvsp_frame, flags, tid)
            {
                Ok(need_signal) => return Ok(need_signal),
                Err(Error::RingFull) => {
                    // Reap TX completions to free recv-ring space so
                    // the host resumes draining our send ring, then
                    // retry the write.
                    let _ = self.drain_inbound(ctx, PENDING_TX_MAX, |_| {});
                }
                Err(e) => return Err(e),
            }
        }
        Err(Error::RingFull)
    }

    /// Spin draining the recv ring until every outstanding TX buffer
    /// has been reclaimed or `max_polls` iterations elapse. Also
    /// acks any xfer-page packets encountered along the way (frames
    /// are handed to `on_frame`).
    ///
    /// Returns [`Error::Timeout`] if the drain didn't complete.
    pub fn flush_tx<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        &mut self,
        ctx: &mut C,
        max_polls: usize,
        mut on_frame: impl FnMut(&[u8]),
    ) -> Result<()> {
        let mut polls = 0usize;
        while !self.pending_tx.is_empty() {
            if polls >= max_polls {
                return Err(Error::Timeout);
            }
            polls += 1;
            self.drain_inbound(ctx, 1, &mut on_frame)?;
        }
        Ok(())
    }

    /// Number of TX buffers whose completion hasn't been observed.
    /// Exposed for smoke-test assertions.
    pub fn pending_tx_len(&self) -> usize {
        self.pending_tx.len()
    }
}

/// Allocate a page-aligned buffer of `size` bytes, register it as a
/// GPADL on `channel_id`, and return an [`OwnedBuf`] carrying the
/// pointer + gpadl handle.
///
/// `size` must be a multiple of 4096. Uses opentmk's static heap via
/// `alloc_zeroed`.
fn allocate_gpadl_buffer<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
    ctx: &mut C,
    channel_id: ChannelId,
    size: usize,
) -> Result<OwnedBuf> {
    if !size.is_multiple_of(4096) || size == 0 {
        return Err(Error::Parse {
            ty: None,
            reason: "GPADL buffer size must be a positive multiple of 4096",
        });
    }
    let layout = Layout::from_size_align(size, 4096).map_err(|_| Error::Parse {
        ty: None,
        reason: "GPADL buffer layout invalid",
    })?;
    // SAFETY: layout is a validated non-zero page-aligned request.
    // Freed via `NetvspBacking::free` after the channel is closed; the
    // host holds it through the GPADL until then.
    #[expect(unsafe_code, reason = "page-aligned allocation for GPADL registration")]
    let ptr = unsafe { alloc_zeroed(layout) };
    if ptr.is_null() {
        return Err(Error::Parse {
            ty: None,
            reason: "GPADL buffer allocation failed",
        });
    }
    let base_gpa = virt_to_phys(ptr);

    let pfn_count = size / 4096;
    let mut pfns: Vec<u64> = Vec::with_capacity(pfn_count);
    for i in 0..pfn_count {
        pfns.push((base_gpa + (i * 4096) as u64) >> 12);
    }
    let gpadl = establish_gpadl(ctx, channel_id, size as u32, &pfns)?;
    Ok(OwnedBuf {
        ptr,
        len: size,
        gpadl,
    })
}
