// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hyper-V synthetic keyboard vdev — the `f912ad6d-...` VMBus
//! device. Protocol constants and wire types are copied from
//! `vm/devices/uidevices/src/keyboard/protocol.rs` (kept in sync
//! manually — this is a `no_std` guest-side view of the same
//! protocol).
//!
//! # Flow
//!
//! 1. Caller identifies the keyboard offer in the
//!    [`OfferChannel`](crate::protocol::OfferChannel) list returned
//!    by [`crate::request_offers`] (interface GUID
//!    [`INTERFACE_GUID`]).
//! 2. Caller allocates a 4-page ring region (send-ctrl, send-data,
//!    recv-ctrl, recv-data), establishes a GPADL over it with
//!    [`crate::gpadl::establish_gpadl`], and opens the channel via
//!    [`crate::channel::open_channel`].
//! 3. Caller wraps the [`Channel`] + two [`RawRingMem`]s in a
//!    [`Keyboard`] and calls [`Keyboard::negotiate_version`].
//! 4. [`Keyboard::poll_keystrokes`] drains any pending keyboard
//!    events; [`Keyboard::set_leds`] tests the send path.
//! 5. Caller [`crate::channel::close_channel`]s the underlying
//!    channel when done.
//!
//! No SINT-based interrupt path is required — polling the recv ring
//! is sufficient (the SynIC is programmed with `polling = true` in
//! [`crate::synic`]).

use crate::Error;
use crate::Result;
use crate::channel::Channel;
use crate::channel::ChannelState;
use crate::ring::RawRingMem;
use crate::ring::RecvRing;
use crate::ring::SendRing;
use opentmk_core::context::HypercallPlatformTrait;
use opentmk_core::platform::hyperv::ctx::HyperVHypercallConfig;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// `f912ad6d-2b17-48ea-bd65-f927a61c7684` — VMBus interface GUID for
/// the Hyper-V synthetic keyboard.
pub const INTERFACE_GUID: crate::protocol::Guid = crate::protocol::Guid {
    data1: 0xf912ad6d,
    data2: 0x2b17,
    data3: 0x48ea,
    data4: [0xbd, 0x65, 0xf9, 0x27, 0xa6, 0x1c, 0x76, 0x84],
};

/// Protocol version — `major << 16 | minor`.
pub const VERSION_WIN8: u32 = 1u32 << 16;

/// Guest request to negotiate a keyboard protocol version.
pub const MESSAGE_PROTOCOL_REQUEST: u32 = 1;
/// Host response to a keyboard protocol version request.
pub const MESSAGE_PROTOCOL_RESPONSE: u32 = 2;
/// Host notification containing a keyboard event.
pub const MESSAGE_EVENT: u32 = 3;
/// Guest request to update the keyboard LED state.
pub const MESSAGE_SET_LED_INDICATORS: u32 = 4;

/// The make code contains a Unicode character rather than a scan code.
pub const KEYSTROKE_IS_UNICODE: u32 = 1 << 0;
/// The event releases a key rather than pressing it.
pub const KEYSTROKE_IS_BREAK: u32 = 1 << 1;
/// The scan code has an `E0` prefix.
pub const KEYSTROKE_IS_E0: u32 = 1 << 2;
/// The scan code has an `E1` prefix.
pub const KEYSTROKE_IS_E1: u32 = 1 << 3;

/// Maximum bytes we expect any keyboard message to occupy (see
/// `MAXIMUM_MESSAGE_SIZE` in the openvmm device).
pub const MAXIMUM_MESSAGE_SIZE: usize = 256;

/// Wire header for every keyboard packet.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct MessageHeader {
    /// Identifies the body layout that follows this header.
    pub message_type: u32,
}

/// `MESSAGE_PROTOCOL_REQUEST` body — guest → host.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct MessageProtocolRequest {
    /// Requested protocol version encoded as `major << 16 | minor`.
    pub version: u32,
}

/// `MESSAGE_PROTOCOL_RESPONSE` body — host → guest.
///
/// `accepted != 0` means the host is willing to speak the requested
/// version.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct MessageProtocolResponse {
    /// Nonzero when the host accepts the requested protocol version.
    pub accepted: u32,
}

/// `MESSAGE_EVENT` body — host → guest, one PS/2 make/break code
/// per packet.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct MessageKeystroke {
    /// PS/2 make code, or a Unicode character when flagged as Unicode.
    pub make_code: u16,
    /// Reserved padding; must be zero.
    pub padding: u16,
    /// Bitwise combination of the `KEYSTROKE_*` flags.
    pub flags: u32,
}

/// `MESSAGE_SET_LED_INDICATORS` body — guest → host. Bit-mask of the
/// LEDs the guest wants lit.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct MessageLedIndicatorsState {
    /// Bit mask describing which keyboard LEDs should be lit.
    pub led_flags: u16,
    /// Reserved padding; must be zero.
    pub padding: u16,
}

/// A parsed inbound keyboard packet.
#[derive(Copy, Clone, Debug)]
pub enum InboundPacket {
    /// `MESSAGE_PROTOCOL_RESPONSE` — from the host.
    ProtocolResponse(MessageProtocolResponse),
    /// `MESSAGE_EVENT` — a keystroke from the host.
    Event(MessageKeystroke),
    /// Any other message type — recorded but not interpreted.
    Other {
        /// Unrecognized wire message type.
        message_type: u32,
        /// Number of bytes following the message header.
        payload_len: usize,
    },
}

/// Handle to an open Hyper-V synthetic keyboard channel.
///
/// Wraps an open [`Channel`] plus the send / receive halves of the
/// ring backing it. All methods are cancellation-safe (they don't
/// keep hidden state across calls beyond what the ring itself
/// already tracks).
pub struct Keyboard {
    channel: Channel,
    send: SendRing<RawRingMem>,
    recv: RecvRing<RawRingMem>,
}

/// Regular data packet — matches `vmbus_ring::PIPE_PACKET_TYPE_DATA`.
/// Kept as a documentation constant even though the Windows Hyper-V
/// synthetic-keyboard vdev uses raw INBAND packets (no PipeHeader).
pub const PIPE_PACKET_TYPE_DATA: u32 = 1;

impl Keyboard {
    /// Wrap an open [`Channel`] with pre-constructed [`RawRingMem`]
    /// views over its send / recv rings. `send_mem` is the ring the
    /// guest writes into (host reads); `recv_mem` is the ring the
    /// host writes into (guest reads).
    pub fn new(channel: Channel, send_mem: RawRingMem, recv_mem: RawRingMem) -> Self {
        Self {
            channel,
            send: SendRing::new(send_mem),
            recv: RecvRing::new(recv_mem),
        }
    }

    /// Send a `MESSAGE_PROTOCOL_REQUEST` and wait for the paired
    /// `MESSAGE_PROTOCOL_RESPONSE`.
    ///
    /// `max_polls` bounds the wait loop — a reasonable value for a
    /// live host is ~10M (a few seconds of spinning). Returns
    /// `Ok(true)` if the host accepted the version, `Ok(false)` if
    /// it rejected, `Err(Error::Timeout)` otherwise.
    pub fn negotiate_version<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        &mut self,
        ctx: &mut C,
        version: u32,
        max_polls: usize,
    ) -> Result<bool> {
        // Build header + body inline (both are 4 bytes -> 8 bytes total).
        let mut payload = [0u8; 8];
        payload[..4].copy_from_slice(
            MessageHeader {
                message_type: MESSAGE_PROTOCOL_REQUEST,
            }
            .as_bytes(),
        );
        payload[4..].copy_from_slice(MessageProtocolRequest { version }.as_bytes());

        self.write_and_signal(ctx, &payload)?;

        log::debug!("keyboard: negotiate_version {version:#x} sent, polling response");
        for _ in 0..max_polls {
            if let Some(packet) = self.recv_next()? {
                if let InboundPacket::ProtocolResponse(resp) = packet {
                    log::info!("keyboard: protocol response accepted={}", resp.accepted);
                    return Ok(resp.accepted != 0);
                }
                log::debug!("keyboard: unexpected packet while waiting for response: {packet:?}");
            }
            core::hint::spin_loop();
        }
        Err(Error::Timeout)
    }

    /// Send a `MESSAGE_SET_LED_INDICATORS` packet. The openvmm host
    /// keyboard accepts (and discards) this — a good round-trip
    /// smoke test that the send / signal path works.
    pub fn set_leds<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        &mut self,
        ctx: &mut C,
        led_flags: u16,
    ) -> Result<()> {
        let mut payload = [0u8; 8];
        payload[..4].copy_from_slice(
            MessageHeader {
                message_type: MESSAGE_SET_LED_INDICATORS,
            }
            .as_bytes(),
        );
        payload[4..].copy_from_slice(
            MessageLedIndicatorsState {
                led_flags,
                padding: 0,
            }
            .as_bytes(),
        );
        self.write_and_signal(ctx, &payload)
    }

    /// Drain and log every packet currently pending on the recv ring,
    /// returning the count of `MESSAGE_EVENT` keystrokes observed.
    pub fn poll_keystrokes(&mut self, max_polls: usize) -> Result<usize> {
        let mut count = 0usize;
        for _ in 0..max_polls {
            match self.recv_next()? {
                Some(InboundPacket::Event(ks)) => {
                    log::info!(
                        "keyboard: keystroke make_code={:#x} flags={:#x}",
                        ks.make_code,
                        ks.flags
                    );
                    count += 1;
                }
                Some(other) => {
                    log::debug!("keyboard: non-event packet {other:?}");
                }
                None => core::hint::spin_loop(),
            }
        }
        Ok(count)
    }

    /// Recover the underlying [`Channel`] for closing.
    pub fn into_channel(self) -> Channel {
        self.channel
    }

    /// Reference to the underlying channel (useful for status checks
    /// and diagnostics).
    pub fn channel(&self) -> &Channel {
        &self.channel
    }

    // ---- internals ----

    fn write_and_signal<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
        &mut self,
        ctx: &mut C,
        payload: &[u8],
    ) -> Result<()> {
        if self.channel.state() != ChannelState::Open {
            return Err(Error::Rescinded);
        }
        // Windows Hyper-V synthetic keyboard uses raw
        // `VM_PKT_DATA_INBAND` packets (no `PipeHeader` framing —
        // that's a MessagePipe convention). Linux's
        // `drivers/input/serio/hyperv-keyboard.c` sends with
        // `VM_PKT_DATA_INBAND` +
        // `VMBUS_DATA_PACKET_FLAG_COMPLETION_REQUESTED`.
        let mut flags = crate::ring::PacketFlags::new();
        flags.set_request_completion(true);
        let need_signal = self.send.write_inband(payload, flags, 0)?;
        let _ = need_signal;
        self.channel.signal(ctx)?;
        Ok(())
    }

    fn recv_next(&self) -> Result<Option<InboundPacket>> {
        let mut buf = [0u8; MAXIMUM_MESSAGE_SIZE];
        match self.recv.read(&mut buf) {
            Ok(pkt) => {
                if pkt.payload.len() < 4 {
                    return Ok(Some(InboundPacket::Other {
                        message_type: 0,
                        payload_len: pkt.payload.len(),
                    }));
                }
                let (hdr, _) =
                    MessageHeader::read_from_prefix(pkt.payload).map_err(|_| Error::Parse {
                        ty: None,
                        reason: "keyboard header parse failed",
                    })?;
                let body = &pkt.payload[size_of::<MessageHeader>()..];
                match hdr.message_type {
                    MESSAGE_PROTOCOL_RESPONSE => {
                        let (resp, _) =
                            MessageProtocolResponse::read_from_prefix(body).map_err(|_| {
                                Error::Parse {
                                    ty: None,
                                    reason: "keyboard protocol response parse failed",
                                }
                            })?;
                        Ok(Some(InboundPacket::ProtocolResponse(resp)))
                    }
                    MESSAGE_EVENT => {
                        let (ks, _) =
                            MessageKeystroke::read_from_prefix(body).map_err(|_| Error::Parse {
                                ty: None,
                                reason: "keyboard event parse failed",
                            })?;
                        Ok(Some(InboundPacket::Event(ks)))
                    }
                    other => Ok(Some(InboundPacket::Other {
                        message_type: other,
                        payload_len: pkt.payload.len(),
                    })),
                }
            }
            Err(Error::RingEmpty) => Ok(None),
            Err(e) => Err(e),
        }
    }
}
