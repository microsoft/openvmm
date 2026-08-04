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

use crate::Error;
use crate::Result;
use crate::gpadl::GpadlHandle;
use crate::protocol::ChannelId;
use crate::protocol::FeatureFlags;
use crate::protocol::MAX_MESSAGE_SIZE;
use crate::protocol::OfferChannel;
use crate::protocol::OpenChannel;
use crate::protocol::OpenChannel2;
use crate::protocol::OpenChannelFlags;
use crate::protocol::OpenResult;
use crate::protocol::RelIdReleased;
use crate::protocol::UserDefinedData;
pub use crate::ring::PacketFlags;
pub use crate::ring::RecvPacket;
use alloc::vec::Vec;
use opentmk::context::HypercallTrait;
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
    /// out of scope for the initial port (§4 signal-path notes).
    ///
    /// The event flag is always `0` for guest→host signals — the
    /// `event_flag` we carry on [`Channel`] is only used for the
    /// **host→guest** direction (bit position in the SIEFP page).
    /// This matches `vmbus_client::guest_to_host_interrupt` in
    /// openvmm which calls `signal_event(connection_id, 0)`.
    pub fn signal<C: HypercallTrait>(&self, ctx: &mut C) -> Result<()> {
        if self.state != ChannelState::Open {
            return Err(Error::Rescinded);
        }
        crate::hypercalls::signal_event(ctx, self.connection_id, 0)
    }
}

/// Allocate a fresh 32-bit `open_id`. Uses a process-wide atomic
/// counter, starting at 1 so a zero id can be used as a sentinel.
pub fn allocate_open_id() -> u32 {
    use core::sync::atomic::AtomicU32;
    use core::sync::atomic::Ordering;
    static NEXT_ID: AtomicU32 = AtomicU32::new(1);
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Encode `OpenChannel` / `OpenChannel2` (depending on negotiated
/// feature flags) for the given parameters. Returns the wire bytes
/// ready to be posted.
pub fn encode_open_channel(
    channel_id: ChannelId,
    open_id: u32,
    ring_gpadl: crate::protocol::GpadlId,
    target_vp: u32,
    downstream_page_offset: u32,
    connection_id: u32,
    event_flag: u16,
    flags: OpenChannelFlags,
    negotiated: FeatureFlags,
) -> Vec<u8> {
    use crate::protocol::HEADER_SIZE;
    use crate::protocol::MessageHeader;
    use crate::protocol::MessageType;
    use crate::protocol::VmbusMessage;
    use core::mem::size_of;

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
    table: &crate::message::CompletionTable,
    pump: &mut P,
    sink: &mut dyn crate::message::MessageSink,
    offer: &OfferChannel,
    ring_gpadl: GpadlHandle,
    send_data_pages: u32,
    connection_id: u32,
    event_flag: u16,
) -> Result<Channel>
where
    C: HypercallTrait,
    P: crate::connection::MessagePump,
{
    let state = crate::connection::connection()
        .clone()
        .ok_or(Error::VersionMismatch)?;

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

    let completion = table.register(crate::message::CompletionKey::OpenChannelResult(open_id));
    crate::hypercalls::post_message(ctx, state.post_message_connection_id, &payload)?;
    pump.poll_until(ctx, &completion, sink)?;
    let bytes = completion.take_response().ok_or(Error::Timeout)?;
    let result: OpenResult = crate::message::parse(&bytes)?;
    if result.status != 0 {
        return Err(Error::Parse {
            ty: Some(crate::protocol::MessageType::OPEN_CHANNEL_RESULT),
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
    C: HypercallTrait,
{
    use crate::protocol::CloseChannel;
    let state = crate::connection::connection()
        .clone()
        .ok_or(Error::VersionMismatch)?;

    let mut buf = [0u8; MAX_MESSAGE_SIZE];

    // Skip the CloseChannel post if the channel has already been
    // rescinded — the host has torn it down for us; we only need to
    // release our end.
    if channel.state != ChannelState::Rescinded {
        let close = CloseChannel {
            channel_id: channel.channel_id,
        };
        let used = crate::message::encode(&close, &mut buf);
        crate::hypercalls::post_message(ctx, state.post_message_connection_id, &buf[..used])?;
    }

    let rel = RelIdReleased {
        channel_id: channel.channel_id,
    };
    let used = crate::message::encode(&rel, &mut buf);
    crate::hypercalls::post_message(ctx, state.post_message_connection_id, &buf[..used])?;

    Ok(())
}

/// UEFI entry point: open `offer` over a caller-supplied ring GPADL
/// and default parameters.
///
/// * `send_data_pages` — power-of-two data pages for the send ring.
/// * `connection_id` / `event_flag` — populated only when Copper's
///   `GUEST_SPECIFIED_SIGNAL_PARAMETERS` feature was negotiated.
pub fn open_channel<C: HypercallTrait>(
    ctx: &mut C,
    offer: &OfferChannel,
    ring_gpadl: GpadlHandle,
    send_data_pages: u32,
    connection_id: u32,
    event_flag: u16,
) -> Result<Channel> {
    let pages = crate::synic::synic_pages().ok_or(Error::VersionMismatch)?;
    let table = crate::message::completion_table();
    let mut pump = crate::interrupt::SimpPump::new(pages.simp_gpa);
    let mut sink = crate::connection::OfferCollector::default();
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
pub fn close_channel<C: HypercallTrait>(ctx: &mut C, channel: Channel) -> Result<()> {
    close_channel_with(ctx, channel)
}
