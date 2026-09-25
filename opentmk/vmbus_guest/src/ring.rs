// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest ring-buffer helpers built on top of [`vmbus_ring::OutgoingRing`]
//! and [`vmbus_ring::IncomingRing`].
//!
//! # Design
//!
//! Upstream `vmbus_ring::OutgoingRing<M>` and
//! `vmbus_ring::IncomingRing<M>` own the ring-buffer state machine:
//! write-index reservation, wrap-around arithmetic, packet-descriptor
//! construction, ordering fences, and the empty→non-empty signal
//! decision. Guest code consumes those types directly.
//!
//! For call-site ergonomics this module provides two extension traits:
//!
//! * [`OutgoingRingExt`] adds `write_inband` / `write_completion` /
//!   `write_gpa_direct` / `write_packet` / `write_raw_packet` /
//!   `set_pending_send_size` on any `OutgoingRing<M>`.
//! * [`IncomingRingExt`] adds `read` / `available` / `set_interrupt_mask`
//!   / `pending_send_size` / `supports_pending_send_size` /
//!   `drain_signal_decision` on any `IncomingRing<M>`.
//!
//! Bring the traits into scope with
//! `use crate::ring::{OutgoingRingExt, IncomingRingExt};` and the
//! methods become available on the concrete ring types.
//!
//! [`RawRingMem`] is guest's implementation of [`RingMem`] over
//! identity-mapped guest-physical pages — used by the UEFI target
//! path. Upstream's own [`FlatRingMem`] backs unit tests via a boxed
//! byte slice.

#![allow(clippy::doc_lazy_continuation)]

use crate::Error;
use crate::Result;
use crate::protocol::GpaDirectHeader;
use crate::protocol::PacketDescriptor;
use crate::protocol::PacketType;
use core::marker::PhantomData;
use core::mem::size_of;
use core::sync::atomic::AtomicU8;
use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering;
use guestmem_core::MemoryRead;
use guestmem_core::MemoryWrite;
use guestmem_core::ranges::PagedRange;
use vmbus_ring::IncomingPacketType;
use vmbus_ring::IncomingRing;
use vmbus_ring::OutgoingPacket;
use vmbus_ring::OutgoingPacketType;
use vmbus_ring::OutgoingRing;
use vmbus_ring::Ring;
use vmbus_ring::WriteError;
use zerocopy::IntoBytes;

pub use crate::protocol::PacketFlags;
pub use vmbus_ring::CONTROL_WORD_COUNT;
pub use vmbus_ring::FlatRingMem;
pub use vmbus_ring::IncomingRing as RecvRing;
pub use vmbus_ring::OutgoingRing as SendRing;
pub use vmbus_ring::RingMem;

/// Feature bit advertising support for the reader-side
/// `pending_send_size` back-pressure protocol.
pub const FEATURE_SUPPORTS_PENDING_SEND_SIZE: u32 = 0x1;

// -- Constants --------------------------------------------------------------

/// Size (in bytes) of the ring control page.
pub const CONTROL_PAGE_SIZE: usize = 4096;

/// Wire size of [`PacketDescriptor`] in bytes.
const DESCRIPTOR_SIZE: usize = size_of::<PacketDescriptor>();

/// Wire size of the ring packet footer.
const FOOTER_SIZE: usize = 8;

/// Word indices in the ring control page (mirrors
/// `vmbus_ring::protocol::Control`).
const IDX_IN: usize = 0;
const IDX_OUT: usize = 1;
const IDX_INTERRUPT_MASK: usize = 2;
const IDX_PENDING_SEND_SZ: usize = 3;
const IDX_FEATURE_BITS: usize = 16;

/// Round `n` up to a multiple of 8.
const fn align8(n: usize) -> usize {
    (n + 7) & !7
}

// -- Return types -----------------------------------------------------------

/// A packet returned from [`IncomingRingExt::read_packet`].
///
/// Presents a guest-typed [`PacketDescriptor`] shape over upstream's
/// [`vmbus_ring::IncomingPacket`]. `packet_type` and `transaction_id`
/// are populated from the upstream parse; the remaining descriptor
/// fields are synthesized from the payload/ext-header sizes.
pub struct RecvPacket<'a> {
    /// Descriptor as observed on the wire.
    pub descriptor: PacketDescriptor,
    /// Payload bytes (without the descriptor or ext header).
    pub payload: &'a [u8],
    /// Length of the extended header in bytes. Non-zero only for
    /// `VM_PKT_DATA_USING_GPA_DIRECT` / `VM_PKT_DATA_USING_XFER_PAGES`
    /// packets; the ext-header bytes sit at `buf[..ext_header_len]`
    /// and the payload at `buf[ext_header_len..]`.
    pub ext_header_len: usize,
}

/// Decision returned by [`IncomingRingExt::drain_signal_decision`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SignalDecision {
    /// Signal the peer — it was blocked on `pending_send_size` and
    /// just crossed the transition to "enough free space".
    Signal,
    /// No signal needed.
    NoSignal,
}

// -- OutgoingRingExt --------------------------------------------------------

/// Guest-side helpers on top of [`vmbus_ring::OutgoingRing`].
pub trait OutgoingRingExt<M: RingMem> {
    /// Post an `VM_PKT_DATA_INBAND` packet with `payload`.
    ///
    /// Returns `Ok(true)` when this write crossed the empty→non-empty
    /// transition and the peer hasn't masked interrupts — callers
    /// should invoke `Channel::signal` only on that transition.
    fn write_inband(&self, payload: &[u8], flags: PacketFlags, transaction_id: u64)
    -> Result<bool>;

    /// Post a completion packet (`VM_PKT_COMP`) referencing
    /// `transaction_id`.
    fn write_completion(&self, payload: &[u8], transaction_id: u64) -> Result<bool>;

    /// Post a `VM_PKT_DATA_USING_GPA_DIRECT` packet with a
    /// single-range GPA-direct extended header referencing an
    /// external, contiguous buffer.
    #[allow(clippy::too_many_arguments)]
    fn write_gpa_direct(
        &self,
        pfns: &[u64],
        byte_offset: u32,
        byte_count: u32,
        payload: &[u8],
        flags: PacketFlags,
        transaction_id: u64,
    ) -> Result<bool>;

    /// Post a packet of arbitrary type. `ext_header` is placed between
    /// the descriptor and payload (used e.g. for GPA-direct headers).
    fn write_packet(
        &self,
        packet_type: PacketType,
        ext_header: &[u8],
        payload: &[u8],
        flags: PacketFlags,
        transaction_id: u64,
    ) -> Result<bool>;

    /// Post a packet with a caller-supplied raw descriptor. Bypasses
    /// upstream's descriptor construction so the caller (typically the
    /// fuzzer) can emit self-inconsistent packets.
    fn write_raw_packet(&self, descriptor: &[u8; DESCRIPTOR_SIZE], payload: &[u8]) -> Result<bool>;

    /// Publish the pending-send-size hint. The peer signals us when
    /// `size` bytes become free.
    fn set_pending_send_size_hint(&self, size: u32);
}

impl<M: RingMem + Sync> OutgoingRingExt<M> for OutgoingRing<M> {
    fn write_inband(
        &self,
        payload: &[u8],
        flags: PacketFlags,
        transaction_id: u64,
    ) -> Result<bool> {
        let typ = if flags.request_completion() {
            OutgoingPacketType::InBandWithCompletion
        } else {
            OutgoingPacketType::InBandNoCompletion
        };
        write_via_upstream(self, transaction_id, typ, payload)
    }

    fn write_completion(&self, payload: &[u8], transaction_id: u64) -> Result<bool> {
        write_via_upstream(
            self,
            transaction_id,
            OutgoingPacketType::Completion,
            payload,
        )
    }

    fn write_gpa_direct(
        &self,
        pfns: &[u64],
        byte_offset: u32,
        byte_count: u32,
        payload: &[u8],
        _flags: PacketFlags,
        transaction_id: u64,
    ) -> Result<bool> {
        // Guest-side validation mirrors what the old inline
        // implementation enforced.
        if pfns.is_empty() {
            return Err(Error::Parse {
                ty: None,
                reason: "gpa-direct requires at least one PFN",
            });
        }
        let offset_u = byte_offset as usize;
        let count_u = byte_count as usize;
        let max = pfns.len() * 0x1000;
        if offset_u >= 0x1000 || count_u == 0 || offset_u + count_u > max {
            return Err(Error::Parse {
                ty: None,
                reason: "gpa-direct byte range does not fit PFN list",
            });
        }
        let range = PagedRange::new(offset_u, count_u, pfns).ok_or(Error::Parse {
            ty: None,
            reason: "gpa-direct: PagedRange::new rejected offset/len/pfns",
        })?;
        let ranges = [range];
        write_via_upstream(
            self,
            transaction_id,
            OutgoingPacketType::GpaDirect(&ranges),
            payload,
        )
    }

    fn write_packet(
        &self,
        packet_type: PacketType,
        ext_header: &[u8],
        payload: &[u8],
        flags: PacketFlags,
        transaction_id: u64,
    ) -> Result<bool> {
        // Not all packet_type values map to upstream's typed variants
        // (upstream only exposes InBand/Completion/GpaDirect/
        // TransferPages). Route the common cases through the typed
        // path and fall through to a raw write for everything else.
        match packet_type {
            PacketType::VM_PKT_DATA_INBAND if ext_header.is_empty() => {
                self.write_inband(payload, flags, transaction_id)
            }
            PacketType::VM_PKT_COMP if ext_header.is_empty() => {
                self.write_completion(payload, transaction_id)
            }
            _ => {
                // Raw path: caller-owned ext_header and packet_type.
                let msg_len = DESCRIPTOR_SIZE + align8(ext_header.len()) + align8(payload.len());
                let desc = PacketDescriptor {
                    packet_type,
                    data_offset8: ((DESCRIPTOR_SIZE + align8(ext_header.len())) / 8) as u16,
                    length8: (msg_len / 8) as u16,
                    flags,
                    transaction_id,
                };
                raw_write(self, desc.as_bytes(), ext_header, payload)
            }
        }
    }

    fn write_raw_packet(&self, descriptor: &[u8; DESCRIPTOR_SIZE], payload: &[u8]) -> Result<bool> {
        raw_write(self, descriptor, &[], payload)
    }

    fn set_pending_send_size_hint(&self, size: u32) {
        self.mem().control()[IDX_PENDING_SEND_SZ].store(size, Ordering::SeqCst);
    }
}

/// Common typed-write path: acquire an offset, write, commit, return
/// the empty→non-empty signal decision.
fn write_via_upstream<M: RingMem>(
    ring: &OutgoingRing<M>,
    transaction_id: u64,
    typ: OutgoingPacketType<'_>,
    payload: &[u8],
) -> Result<bool> {
    let mut off = ring.outgoing().map_err(map_ring_err)?;
    let packet = OutgoingPacket {
        transaction_id,
        size: payload.len(),
        typ,
    };
    let range = match ring.write(&mut off, &packet) {
        Ok(r) => r,
        Err(WriteError::Full(need)) => {
            // Publish pending_send_size so the reader wakes us when
            // space opens up.
            let hint = need.min(u32::MAX as usize) as u32;
            ring.mem().control()[IDX_PENDING_SEND_SZ].store(hint, Ordering::SeqCst);
            return Err(Error::RingFull);
        }
        Err(WriteError::Corrupt(_)) => {
            return Err(Error::Parse {
                ty: None,
                reason: "upstream OutgoingRing::write returned a corrupt-ring error",
            });
        }
    };
    // Clear any stale pending_send_size hint from a prior full-ring
    // event now that the write succeeded.
    ring.mem().control()[IDX_PENDING_SEND_SZ].store(0, Ordering::SeqCst);
    if !payload.is_empty() {
        range
            .writer(ring)
            .write(payload)
            .map_err(|_| Error::Parse {
                ty: None,
                reason: "writer over RingRange refused payload",
            })?;
    }
    Ok(ring.commit_write(&mut off))
}

/// Low-level write bypassing upstream's descriptor construction.
///
/// Manages the write-index directly on the `RingMem` control page so
/// callers (namely `write_raw_packet` and the arbitrary-type arm of
/// `write_packet`) can inject descriptors upstream would reject.
fn raw_write<M: RingMem>(
    ring: &OutgoingRing<M>,
    descriptor: &[u8],
    ext_header: &[u8],
    payload: &[u8],
) -> Result<bool> {
    assert_eq!(descriptor.len(), DESCRIPTOR_SIZE);
    let ext_padded = align8(ext_header.len());
    let payload_padded = align8(payload.len());
    let total = DESCRIPTOR_SIZE + ext_padded + payload_padded + FOOTER_SIZE;
    let mem = ring.mem();
    let ring_len = mem.len() as u32;
    let ctrl = mem.control();
    let write_idx = ctrl[IDX_IN].load(Ordering::Relaxed);
    let read_idx = ctrl[IDX_OUT].load(Ordering::Acquire);
    let free = available_free(write_idx, read_idx, ring_len) as usize;
    if free < total {
        ctrl[IDX_PENDING_SEND_SZ].store(total as u32, Ordering::SeqCst);
        let read_idx = ctrl[IDX_OUT].load(Ordering::SeqCst);
        let free = available_free(write_idx, read_idx, ring_len) as usize;
        if free < total {
            return Err(Error::RingFull);
        }
        ctrl[IDX_PENDING_SEND_SZ].store(0, Ordering::SeqCst);
    }

    let mut cursor = write_idx as usize;
    mem.write_at(cursor, descriptor);
    cursor += DESCRIPTOR_SIZE;
    if !ext_header.is_empty() {
        mem.write_at(cursor, ext_header);
        cursor += ext_header.len();
        if ext_padded > ext_header.len() {
            let zeros = [0u8; 8];
            mem.write_at(cursor, &zeros[..ext_padded - ext_header.len()]);
            cursor += ext_padded - ext_header.len();
        }
    }
    if !payload.is_empty() {
        mem.write_at(cursor, payload);
        cursor += payload.len();
        if payload_padded > payload.len() {
            let zeros = [0u8; 8];
            mem.write_at(cursor, &zeros[..payload_padded - payload.len()]);
            cursor += payload_padded - payload.len();
        }
    }
    // Footer: reserved u32 + starting write_idx u32.
    let mut footer = [0u32; 2];
    footer[1] = write_idx;
    mem.write_at(cursor, footer.as_bytes());

    let new_write_idx = (write_idx + total as u32) & (ring_len - 1);
    ctrl[IDX_IN].store(new_write_idx, Ordering::SeqCst);
    let read_after = ctrl[IDX_OUT].load(Ordering::SeqCst);
    let was_empty = read_after == write_idx;
    let peer_wants_signal = ctrl[IDX_INTERRUPT_MASK].load(Ordering::SeqCst) == 0;
    Ok(was_empty && peer_wants_signal)
}

// -- IncomingRingExt --------------------------------------------------------

/// Guest-side helpers on top of [`vmbus_ring::IncomingRing`].
pub trait IncomingRingExt<M: RingMem> {
    /// Read one packet into `buf`. Returns `Err(Error::RingEmpty)` if
    /// the ring is empty.
    ///
    /// Named `read_packet` (rather than `read`) to avoid shadowing
    /// [`IncomingRing::read`], which has an incompatible signature
    /// (`&mut IncomingOffset` instead of a byte buffer).
    fn read_packet<'a>(&self, buf: &'a mut [u8]) -> Result<RecvPacket<'a>>;

    /// Number of bytes available to read right now.
    fn available(&self) -> u32;

    /// Mask host→guest signalling.
    fn set_interrupt_mask_hint(&self, masked: bool);

    /// Whether the ring's writer (the host, for a RecvRing) has
    /// advertised support for the `pending_send_size` protocol.
    fn supports_pending_send_size_hint(&self) -> bool;

    /// Read the writer's current pending-send-size hint. Non-zero
    /// means the writer is blocked waiting for at least this many
    /// free bytes.
    fn pending_send_size_hint(&self) -> u32;

    /// Compute the reader-side signal decision after `bytes_read`
    /// bytes have been advanced past `read_index`.
    fn drain_signal_decision(&self, bytes_read: u32) -> SignalDecision;
}

impl<M: RingMem + Sync> IncomingRingExt<M> for IncomingRing<M> {
    fn read_packet<'a>(&self, buf: &'a mut [u8]) -> Result<RecvPacket<'a>> {
        let mut off = self.incoming().map_err(map_ring_err)?;
        let pkt = IncomingRing::read(self, &mut off).map_err(|e| match e {
            vmbus_ring::ReadError::Empty => Error::RingEmpty,
            vmbus_ring::ReadError::Corrupt(_) => Error::Parse {
                ty: None,
                reason: "upstream IncomingRing::read returned a corrupt-ring error",
            },
        })?;
        // Synthesize a guest-shaped RecvPacket. Descriptor's typed
        // fields (packet_type / flags / transaction_id) come from the
        // upstream parse; length8 / data_offset8 are computed from
        // the buffer geometry.
        let (packet_type, ext_hdr_bytes) = match &pkt.typ {
            IncomingPacketType::InBand => (PacketType::VM_PKT_DATA_INBAND, 0),
            IncomingPacketType::Completion => (PacketType::VM_PKT_COMP, 0),
            IncomingPacketType::GpaDirect(_, ext) => (
                PacketType::VM_PKT_DATA_USING_GPA_DIRECT,
                size_of::<GpaDirectHeader>() + ext.len(),
            ),
            IncomingPacketType::TransferPages(_, _, ext) => (
                PacketType::VM_PKT_DATA_USING_XFER_PAGES,
                size_of::<crate::protocol::TransferPageHeader>() + ext.len(),
            ),
        };
        let payload_len = pkt.payload.len();
        let ext_header_len = ext_hdr_bytes;
        let needed = ext_header_len + payload_len;
        if buf.len() < needed {
            return Err(Error::Parse {
                ty: None,
                reason: "recv buffer smaller than packet payload",
            });
        }
        // Read ext_header (if any) into buf[..ext_header_len].
        match &pkt.typ {
            IncomingPacketType::InBand | IncomingPacketType::Completion => {}
            IncomingPacketType::GpaDirect(range_count, ext_range) => {
                let hdr = GpaDirectHeader {
                    reserved: 0,
                    range_count: *range_count,
                };
                let hlen = size_of::<GpaDirectHeader>();
                buf[..hlen].copy_from_slice(hdr.as_bytes());
                ext_range
                    .reader(self)
                    .read(&mut buf[hlen..ext_header_len])
                    .map_err(|_| Error::Parse {
                        ty: None,
                        reason: "reader over GpaDirect ext-header range refused",
                    })?;
            }
            IncomingPacketType::TransferPages(id, range_count, ext_range) => {
                let hdr = crate::protocol::TransferPageHeader {
                    transfer_page_set_id: *id,
                    reserved: 0,
                    range_count: *range_count,
                };
                let hlen = size_of::<crate::protocol::TransferPageHeader>();
                buf[..hlen].copy_from_slice(hdr.as_bytes());
                ext_range
                    .reader(self)
                    .read(&mut buf[hlen..ext_header_len])
                    .map_err(|_| Error::Parse {
                        ty: None,
                        reason: "reader over TransferPages ext-header range refused",
                    })?;
            }
        }
        // Read payload into buf[ext_header_len..needed].
        if payload_len > 0 {
            pkt.payload
                .reader(self)
                .read(&mut buf[ext_header_len..needed])
                .map_err(|_| Error::Parse {
                    ty: None,
                    reason: "reader over payload range refused",
                })?;
        }
        let _need_signal = self.commit_read(&mut off);
        let msg_len = DESCRIPTOR_SIZE + ext_header_len + align8(payload_len);
        let mut flags = PacketFlags::new();
        // Preserve the completion-requested bit: upstream's parse
        // sets `transaction_id.is_some()` iff the packet had
        // PACKET_FLAG_COMPLETION_REQUESTED (or was a Completion).
        if pkt.transaction_id.is_some() && !matches!(pkt.typ, IncomingPacketType::Completion) {
            flags.set_request_completion(true);
        }
        let descriptor = PacketDescriptor {
            packet_type,
            data_offset8: ((DESCRIPTOR_SIZE + ext_header_len) / 8) as u16,
            length8: (msg_len / 8) as u16,
            flags,
            transaction_id: pkt.transaction_id.unwrap_or(0),
        };
        Ok(RecvPacket {
            descriptor,
            payload: &buf[ext_header_len..needed],
            ext_header_len,
        })
    }

    fn available(&self) -> u32 {
        let ctrl = self.mem().control();
        let write_idx = ctrl[IDX_IN].load(Ordering::Acquire);
        let read_idx = ctrl[IDX_OUT].load(Ordering::Relaxed);
        available_data(write_idx, read_idx, self.mem().len() as u32)
    }

    fn set_interrupt_mask_hint(&self, masked: bool) {
        self.mem().control()[IDX_INTERRUPT_MASK].store(masked as u32, Ordering::Release);
    }

    fn supports_pending_send_size_hint(&self) -> bool {
        let bits = self.mem().control()[IDX_FEATURE_BITS].load(Ordering::Relaxed);
        (bits & FEATURE_SUPPORTS_PENDING_SEND_SIZE) != 0
    }

    fn pending_send_size_hint(&self) -> u32 {
        self.mem().control()[IDX_PENDING_SEND_SZ].load(Ordering::SeqCst)
    }

    fn drain_signal_decision(&self, bytes_read: u32) -> SignalDecision {
        if !self.supports_pending_send_size_hint() {
            return SignalDecision::NoSignal;
        }
        let pending = self.pending_send_size_hint();
        if pending == 0 {
            return SignalDecision::NoSignal;
        }
        let ring_len = self.mem().len() as u32;
        let ctrl = self.mem().control();
        let write_idx = ctrl[IDX_IN].load(Ordering::SeqCst);
        let read_idx = ctrl[IDX_OUT].load(Ordering::SeqCst);
        let new_free = available_free(write_idx, read_idx, ring_len);
        let old_free = new_free.saturating_sub(bytes_read);
        if old_free < pending && new_free >= pending {
            SignalDecision::Signal
        } else {
            SignalDecision::NoSignal
        }
    }
}

// -- RawRingMem -------------------------------------------------------------

/// A [`RingMem`] backed by two raw pointers into identity-mapped
/// guest-physical memory.
///
/// Unlike upstream's [`FlatRingMem`] this does not own the pages —
/// the caller must keep them alive and guarantee the layout matches
/// the VMBus wire format: the `control` pointer references a 4 KiB
/// control page whose first [`CONTROL_WORD_COUNT`] `u32` slots hold
/// the ring indices, and `data` points at `data_len` bytes of
/// contiguous data pages (power-of-two).
///
/// # Safety
///
/// The caller MUST guarantee that:
/// * `control` and `data` are valid, well-aligned pointers to memory
///   that lives at least as long as this `RawRingMem`.
/// * The memory is not aliased by any Rust reference (only via
///   `RawRingMem` for its lifetime).
/// * `data_len` is a power of two and does not exceed the actual
///   allocation.
pub struct RawRingMem {
    control: *const AtomicU32,
    data: *const AtomicU8,
    data_len: usize,
    _marker: PhantomData<()>,
}

// SAFETY: All access goes through atomic operations on `*const AtomicU8`
// / `*const AtomicU32`. There is no interior state that requires
// synchronisation beyond what the caller has already committed to by
// handing us the pointers.
#[expect(unsafe_code, reason = "raw-pointer-backed ring memory for UEFI target")]
unsafe impl Send for RawRingMem {}
#[expect(unsafe_code, reason = "raw-pointer-backed ring memory for UEFI target")]
unsafe impl Sync for RawRingMem {}

impl RawRingMem {
    /// Construct a new [`RawRingMem`] over identity-mapped pages.
    ///
    /// # Safety
    ///
    /// See the type-level docs — the caller vouches for pointer
    /// validity, exclusive access, and the layout invariants.
    #[expect(unsafe_code, reason = "raw-pointer constructor for UEFI target")]
    pub unsafe fn new(control: *const AtomicU32, data: *const AtomicU8, data_len: usize) -> Self {
        assert!(data_len.is_power_of_two() && data_len >= 8);
        Self {
            control,
            data,
            data_len,
            _marker: PhantomData,
        }
    }
}

impl RingMem for RawRingMem {
    fn control(&self) -> &[AtomicU32; CONTROL_WORD_COUNT] {
        // SAFETY: caller of `new` guaranteed the control pointer is
        // valid for at least `CONTROL_WORD_COUNT` `AtomicU32`s.
        #[expect(unsafe_code, reason = "materialise array over control page")]
        unsafe {
            &*self.control.cast::<[AtomicU32; CONTROL_WORD_COUNT]>()
        }
    }

    fn read_at(&self, mut addr: usize, data: &mut [u8]) {
        // Contract: addr + data.len() <= data_len * 2 (wrap once).
        if addr >= self.data_len {
            addr -= self.data_len;
        }
        let mask = self.data_len - 1;
        for (i, byte) in data.iter_mut().enumerate() {
            // SAFETY: caller of `new` guaranteed data is valid for
            // `data_len` bytes; masking keeps the index in range.
            #[expect(unsafe_code, reason = "raw ring data read")]
            unsafe {
                *byte = (*self.data.add((addr + i) & mask)).load(Ordering::Relaxed);
            }
        }
    }

    fn write_at(&self, mut addr: usize, data: &[u8]) {
        if addr >= self.data_len {
            addr -= self.data_len;
        }
        let mask = self.data_len - 1;
        for (i, byte) in data.iter().enumerate() {
            // SAFETY: as in read_at.
            #[expect(unsafe_code, reason = "raw ring data write")]
            unsafe {
                (*self.data.add((addr + i) & mask)).store(*byte, Ordering::Relaxed);
            }
        }
    }

    fn len(&self) -> usize {
        self.data_len
    }
}

// -- Free-space accounting --------------------------------------------------

fn available_free(write_idx: u32, read_idx: u32, ring_len: u32) -> u32 {
    if write_idx >= read_idx {
        ring_len - (write_idx - read_idx) - 8
    } else {
        read_idx - write_idx - 8
    }
}

fn available_data(write_idx: u32, read_idx: u32, ring_len: u32) -> u32 {
    if write_idx >= read_idx {
        write_idx - read_idx
    } else {
        ring_len - (read_idx - write_idx)
    }
}

// -- Error mapping ----------------------------------------------------------

fn map_ring_err(err: vmbus_ring::Error) -> Error {
    Error::Parse {
        ty: None,
        reason: match err {
            vmbus_ring::Error::InvalidRingMemory => "invalid ring memory",
            vmbus_ring::Error::InvalidRingPointer => "invalid ring pointers",
            vmbus_ring::Error::InvalidMessageLength => "invalid message length",
            _ => "upstream vmbus_ring error",
        },
    }
}
