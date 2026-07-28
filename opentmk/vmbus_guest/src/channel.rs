// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Channel lifecycle (open, close, rel-id-released) and the
//! [`Channel`] handle exposed to consumers.
//!
//! Public API mirrors the shape described in §2 of the design doc.

use crate::Error;
use crate::Result;
use crate::gpadl::GpadlHandle;
use crate::protocol::ChannelId;
use crate::protocol::OfferChannel;
use crate::ring::PacketFlags;
use crate::ring::RecvPacket;
use opentmk::context::HypercallTrait;

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
/// Owned by the caller; drop-time behaviour is the caller's
/// responsibility (call [`close_channel`] explicitly). See §6 of the
/// design doc for rescind semantics.
#[expect(
    dead_code,
    reason = "scaffold: fields will be read once ring send/recv lands"
)]
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

    /// Current lifecycle state.
    pub fn state(&self) -> ChannelState {
        self.state
    }

    /// Send an inband packet (`VM_PKT_DATA_INBAND`).
    ///
    /// **Not implemented in the scaffold.**
    pub fn send_inband<C: HypercallTrait>(
        &mut self,
        _ctx: &mut C,
        _payload: &[u8],
        _flags: PacketFlags,
    ) -> Result<()> {
        Err(Error::NotImplemented)
    }

    /// Send a `VM_PKT_DATA_USING_GPA_DIRECT` packet.
    ///
    /// **Not implemented in the scaffold.**
    pub fn send_gpa_direct<C: HypercallTrait>(
        &mut self,
        _ctx: &mut C,
        _hdr: &[u8],
        _ranges: &[crate::protocol::GpaRange],
    ) -> Result<()> {
        Err(Error::NotImplemented)
    }

    /// Block for the next packet on the recv ring.
    ///
    /// **Not implemented in the scaffold.**
    pub fn recv<'a, C: HypercallTrait>(
        &mut self,
        _ctx: &mut C,
        _buf: &'a mut [u8],
    ) -> Result<RecvPacket<'a>> {
        Err(Error::NotImplemented)
    }

    /// Non-blocking variant of [`Channel::recv`].
    ///
    /// **Not implemented in the scaffold.**
    pub fn try_recv<'a, C: HypercallTrait>(
        &mut self,
        _ctx: &mut C,
        _buf: &'a mut [u8],
    ) -> Result<Option<RecvPacket<'a>>> {
        Err(Error::NotImplemented)
    }
}

/// Open the specified `offer` with `ring_pages` pages of send + recv ring.
///
/// **Not implemented in the scaffold.** See §4 step 7 of the design.
pub fn open_channel<C: HypercallTrait>(
    _ctx: &mut C,
    _offer: &OfferChannel,
    _ring_pages: usize,
) -> Result<Channel> {
    Err(Error::NotImplemented)
}

/// Close a previously opened channel, tear down the ring GPADL, and
/// release the rel-id.
///
/// **Not implemented in the scaffold.**
pub fn close_channel<C: HypercallTrait>(_ctx: &mut C, _channel: Channel) -> Result<()> {
    Err(Error::NotImplemented)
}
