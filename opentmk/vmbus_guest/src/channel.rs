// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Channel lifecycle: [`open_channel`] / [`close_channel`] +
//! [`RelIdReleased`] cleanup.
//!
//! The wire-level state machine (post `OpenChannel[/2]`, wait for
//! `OpenResult`) is factored into [`open_channel_with`] /
//! [`close_channel_with`] which take an explicit
//! [`crate::message::CompletionTable`] and
//! [`crate::connection::MessagePump`]. The UEFI entry points wrap
//! those with the process-wide table and SIMP pump.
//!
//! Ring-buffer memory ownership belongs to the caller: [`Channel`]
//! records the GPADL handle it was opened over but does **not**
//! allocate or free the pages. This keeps the state machine
//! host-testable and lets callers plug in either the
//! [`crate::ring::OwnedRingMem`] host allocator or a UEFI page
//! allocation.
//!
//! # Typical use
//!
//! Most consumers should use the vdev helpers instead of talking to
//! [`Channel`] directly:
//! * [`crate::devices::keyboard::Keyboard`] — synthetic keyboard.
//! * [`crate::devices::netvsp::Netvsp::open`] — synthetic NIC.
//!
//! Reach for [`open_channel`] here only when adding a new vdev
//! driver or exercising the raw state machine.
//!
//! # Example (raw channel over an owned ring)
//!
//! ```ignore
//! use vmbus_guest::{channel, gpadl, protocol::ChannelId};
//! use vmbus_guest::ring::{RawRingMem, SendRing, RecvRing};
//!
//! // 1. Allocate contiguous ring pages (send ctrl + send data + recv ctrl + recv data).
//! //    The caller owns this allocation for the lifetime of the channel.
//! # let (base_ptr, region_bytes, pfns, data_pages) = todo!();
//!
//! // 2. Register a GPADL for the whole region.
//! let g = gpadl::establish_gpadl(&mut ctx, offer.channel_id, region_bytes as u32, &pfns)?;
//!
//! // 3. Open the channel. `send_data_pages` tells the host where the
//! //    send ring ends and the recv ring begins.
//! let ch = channel::open_channel(
//!     &mut ctx,
//!     &offer,
//!     g,
//!     data_pages,
//!     offer.connection_id,
//!     offer.channel_id.0 as u16,
//! )?;
//!
//! // 4. Wrap send + recv halves in the ring API.
//! // let send = SendRing::new(unsafe { RawRingMem::new(send_ctrl, send_data, data_bytes) });
//! // let recv = RecvRing::new(unsafe { RawRingMem::new(recv_ctrl, recv_data, data_bytes) });
//!
//! // 5. After every send.write_* call, signal the host:
//! ch.signal(&mut ctx)?;
//!
//! // 6. On shutdown:
//! channel::close_channel(&mut ctx, ch)?;
//! # Ok::<_, vmbus_guest::Error>(())
//! ```
//!
//! Note that `Channel` doesn't own the ring memory or the GPADL
//! lifetime beyond recording the handle. If you close a channel you
//! must also tear down its GPADL via [`crate::gpadl::teardown_gpadl`]
//! and free the backing pages yourself.

use crate::Error;
use crate::Result;
use crate::connection::MessagePump;
use crate::connection::OfferCollector;
use crate::connection::connection;
use crate::gpadl::GpadlHandle;
use crate::hypercalls::post_message;
use crate::hypercalls::signal_event;
use crate::interrupt::SimpPump;
use crate::message::CompletionKey;
use crate::message::CompletionTable;
use crate::message::MessageSink;
use crate::message::completion_table;
use crate::message::encode;
use crate::message::parse;
use crate::protocol::ChannelId;
use crate::protocol::CloseChannel;
use crate::protocol::FeatureFlags;
use crate::protocol::GpadlId;
use crate::protocol::HEADER_SIZE;
use crate::protocol::MAX_MESSAGE_SIZE;
use crate::protocol::MessageHeader;
use crate::protocol::MessageType;
use crate::protocol::OfferChannel;
use crate::protocol::OpenChannel;
use crate::protocol::OpenChannel2;
use crate::protocol::OpenChannelFlags;
use crate::protocol::OpenResult;
use crate::protocol::RelIdReleased;
use crate::protocol::UserDefinedData;
use crate::protocol::VmbusMessage;
pub use crate::ring::PacketFlags;
pub use crate::ring::RecvPacket;
use crate::synic::synic_pages;
use alloc::vec::Vec;
use core::mem::size_of;
use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering;
use opentmk_core::context::HypercallPlatformTrait;
use opentmk_core::platform::hyperv::ctx::HyperVHypercallConfig;
use zerocopy::IntoBytes;

/// Whether the channel is usable.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ChannelState {
    /// The channel is open and ring send/recv is allowed.
    Open,
    /// The host has rescinded the channel; further operations must
    /// return [`Error::Rescinded`].
    Rescinded,
    /// The channel has been closed cleanly.
    Closed,
}

/// A guest-side handle to a VMBus channel.
///
/// Ownership of the ring memory sits with the caller — [`Channel`]
/// only records the [`GpadlHandle`] and the ids the host assigned.
/// Drop the value only after [`close_channel`] (or after an observed
/// rescind).
#[derive(Debug)]
pub struct Channel {
    pub(crate) channel_id: ChannelId,
    pub(crate) open_id: u32,
    pub(crate) ring_gpadl: GpadlHandle,
    pub(crate) connection_id: u32,
    pub(crate) event_flag: u16,
    pub(crate) state: ChannelState,
}

impl Channel {
    /// The host-assigned channel id.
    pub fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    /// The `open_id` we used when opening this channel.
    pub fn open_id(&self) -> u32 {
        self.open_id
    }

    /// The GPADL backing the ring buffer.
    pub fn ring_gpadl(&self) -> GpadlHandle {
        self.ring_gpadl
    }

    /// Connection id used to signal the host on send.
    pub fn connection_id(&self) -> u32 {
        self.connection_id
    }

    /// Event flag used to signal the host on send.
    pub fn event_flag(&self) -> u16 {
        self.event_flag
    }

    /// Current lifecycle state.
    pub fn state(&self) -> ChannelState {
        self.state
    }

    /// Signal the host that we've published data on the send ring.
    ///
    /// Only invokes `HvSignalEvent`; monitor-page-based signalling is
    /// out of scope for this port (it's a Copper+ optimisation that
    /// avoids a hypercall by touching a shared monitor page instead).
    ///
    /// The event flag is always `0` for guest→host signals — the
    /// `event_flag` we carry on [`Channel`] is only used for the
    /// **host→guest** direction (bit position in the SIEFP page).
    /// This matches `vmbus_client::guest_to_host_interrupt` in
    /// openvmm which calls `signal_event(connection_id, 0)`.
    pub fn signal<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        &self,
        ctx: &mut C,
    ) -> Result<()> {
        if self.state != ChannelState::Open {
            return Err(Error::Rescinded);
        }
        signal_event(ctx, self.connection_id, 0)
    }
}

/// Allocate a fresh 32-bit `open_id`. Uses a process-wide atomic
/// counter, starting at 1 so a zero id can be used as a sentinel.
pub fn allocate_open_id() -> u32 {
    static NEXT_ID: AtomicU32 = AtomicU32::new(1);
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Encode `OpenChannel` / `OpenChannel2` (depending on negotiated
/// feature flags) for the given parameters. Returns the wire bytes
/// ready to be posted.
pub fn encode_open_channel(
    channel_id: ChannelId,
    open_id: u32,
    ring_gpadl: GpadlId,
    target_vp: u32,
    downstream_page_offset: u32,
    connection_id: u32,
    event_flag: u16,
    flags: OpenChannelFlags,
    negotiated: FeatureFlags,
) -> Vec<u8> {
    let base = OpenChannel {
        channel_id,
        open_id,
        ring_buffer_gpadl_id: ring_gpadl,
        target_vp,
        downstream_ring_buffer_page_offset: downstream_page_offset,
        user_data: UserDefinedData::default(),
    };
    let use_v2 = negotiated.guest_specified_signal_parameters()
        || negotiated.channel_interrupt_redirection();

    let mut buf = Vec::new();
    if use_v2 {
        let msg = OpenChannel2 {
            open_channel: base,
            connection_id,
            event_flag,
            flags,
        };
        buf.reserve(HEADER_SIZE + size_of::<OpenChannel2>());
        buf.extend_from_slice(MessageHeader::new(MessageType::OPEN_CHANNEL).as_bytes());
        buf.extend_from_slice(msg.as_bytes());
    } else {
        buf.reserve(HEADER_SIZE + size_of::<OpenChannel>());
        buf.extend_from_slice(
            MessageHeader::new(<OpenChannel as VmbusMessage>::MESSAGE_TYPE).as_bytes(),
        );
        buf.extend_from_slice(base.as_bytes());
    }
    buf
}

/// Pump-based [`open_channel`] the host tests can drive.
///
/// * `offer` — the offer chosen for opening.
/// * `ring_gpadl` — GPADL handle covering the ring buffer memory.
///   `ring_pages` describes its layout: `1 + send_data_pages`
///   contiguous pages for the send ring, then `1 + recv_data_pages`
///   for the recv ring. The `downstream_ring_buffer_page_offset`
///   field posted to the host is `1 + send_data_pages`.
/// * `send_data_pages` — data pages for the send ring (must be
///   power-of-two).
pub fn open_channel_with<C, P>(
    ctx: &mut C,
    table: &CompletionTable,
    pump: &mut P,
    sink: &mut dyn MessageSink,
    offer: &OfferChannel,
    ring_gpadl: GpadlHandle,
    send_data_pages: u32,
    connection_id: u32,
    event_flag: u16,
) -> Result<Channel>
where
    C: HypercallPlatformTrait<Config = HyperVHypercallConfig>,
    P: MessagePump,
{
    let state = connection().clone().ok_or(Error::VersionMismatch)?;

    let open_id = allocate_open_id();
    let downstream_page_offset = 1 + send_data_pages;
    // target_vp = u32::MAX (VP_INDEX_DISABLE_INTERRUPT) — tell the host
    // not to inject an interrupt on host→guest signals; we're polling
    // the recv ring anyway. Matches `vmbus_client`'s
    // `open_data.target_vp.unwrap_or(VP_INDEX_DISABLE_INTERRUPT)`.
    let payload = encode_open_channel(
        offer.channel_id,
        open_id,
        ring_gpadl.gpadl_id,
        u32::MAX,
        downstream_page_offset,
        connection_id,
        event_flag,
        OpenChannelFlags::new(),
        state.feature_flags,
    );

    let completion = table.register(CompletionKey::OpenChannelResult(open_id));
    post_message(ctx, state.post_message_connection_id, &payload)?;
    pump.poll_until(ctx, &completion, sink)?;
    let bytes = completion.take_response().ok_or(Error::Timeout)?;
    let result: OpenResult = parse(&bytes)?;
    if result.status != 0 {
        return Err(Error::Parse {
            ty: Some(MessageType::OPEN_CHANNEL_RESULT),
            reason: "host returned non-success OpenResult status",
        });
    }

    Ok(Channel {
        channel_id: offer.channel_id,
        open_id,
        ring_gpadl,
        connection_id,
        event_flag,
        state: ChannelState::Open,
    })
}

/// Pump-based [`close_channel`].
///
/// Posts `CloseChannel`, then `RelIdReleased`. Neither requires a
/// completion — the host tears the channel down synchronously.
pub fn close_channel_with<C>(ctx: &mut C, channel: Channel) -> Result<()>
where
    C: HypercallPlatformTrait<Config = HyperVHypercallConfig>,
{
    let state = connection().clone().ok_or(Error::VersionMismatch)?;

    let mut buf = [0u8; MAX_MESSAGE_SIZE];

    // Skip the CloseChannel post if the channel has already been
    // rescinded — the host has torn it down for us; we only need to
    // release our end.
    if channel.state != ChannelState::Rescinded {
        let close = CloseChannel {
            channel_id: channel.channel_id,
        };
        let used = encode(&close, &mut buf);
        post_message(ctx, state.post_message_connection_id, &buf[..used])?;
    }

    let rel = RelIdReleased {
        channel_id: channel.channel_id,
    };
    let used = encode(&rel, &mut buf);
    post_message(ctx, state.post_message_connection_id, &buf[..used])?;

    Ok(())
}

/// UEFI entry point: open `offer` over a caller-supplied ring GPADL
/// and default parameters.
///
/// * `send_data_pages` — power-of-two data pages for the send ring.
/// * `connection_id` / `event_flag` — populated only when Copper's
///   `GUEST_SPECIFIED_SIGNAL_PARAMETERS` feature was negotiated.
pub fn open_channel<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
    ctx: &mut C,
    offer: &OfferChannel,
    ring_gpadl: GpadlHandle,
    send_data_pages: u32,
    connection_id: u32,
    event_flag: u16,
) -> Result<Channel> {
    let pages = synic_pages().ok_or(Error::VersionMismatch)?;
    let table = completion_table();
    let mut pump = SimpPump::new(pages.simp_gpa);
    let mut sink = OfferCollector::default();
    open_channel_with(
        ctx,
        table,
        &mut pump,
        &mut sink,
        offer,
        ring_gpadl,
        send_data_pages,
        connection_id,
        event_flag,
    )
}

/// UEFI entry point: [`close_channel_with`].
pub fn close_channel<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
    ctx: &mut C,
    channel: Channel,
) -> Result<()> {
    close_channel_with(ctx, channel)
}
