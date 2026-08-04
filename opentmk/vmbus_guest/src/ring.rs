// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Ring-buffer send / receive.
//!
//! Layout follows Linux `drivers/hv/ring_buffer.c` and openvmm's
//! `vmbus_ring`. Each half of a full-duplex channel has a **control
//! page** (4 KiB) at offset 0 and a **data area** of N contiguous pages
//! after it:
//!
//! ```text
//! +----------------------+  page 0 (control page)
//! | word[0] write_index  |  aka `in`
//! | word[1] read_index   |  aka `out`
//! | word[2] interrupt_mask|
//! | word[3] pending_send_sz|
//! | ...                  |
//! | word[16] feature_bits|
//! +----------------------+  page 1 .. N (data pages)
//! | packet_descriptor    |
//! | optional ext header  |
//! | payload (8-aligned)  |
//! | footer               |
//! | ... next packet ...  |
//! +----------------------+
//! ```
//!
//! The writer signals the host **only** on an empty → non-empty
//! transition when `interrupt_mask == 0` (see §4 "Ring buffer
//! conventions" of `tasks/vmbus-port-design.md`).

use crate::Error;
use crate::Result;
use crate::protocol::PacketDescriptor;
use crate::protocol::PacketType;
use alloc::boxed::Box;
use alloc::vec;
use core::mem::size_of;
use core::sync::atomic::AtomicU8;
use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

pub use crate::protocol::PacketFlags;

/// Control-page word indices (matches openvmm `Control`).
const IDX_IN: usize = 0;
const IDX_OUT: usize = 1;
const IDX_INTERRUPT_MASK: usize = 2;
const IDX_PENDING_SEND_SZ: usize = 3;
const IDX_FEATURE_BITS: usize = 16;

/// Number of `u32` words the control page exposes.
pub const CONTROL_WORD_COUNT: usize = IDX_FEATURE_BITS + 1;

/// Size (in bytes) of the ring control page.
pub const CONTROL_PAGE_SIZE: usize = 4096;

/// Packet footer: reserved word followed by the ring offset of the
/// packet, both `u32`. See `openvmm/vmbus_ring::Footer`.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    zerocopy::IntoBytes,
    zerocopy::FromBytes,
    zerocopy::Immutable,
    zerocopy::KnownLayout,
)]
struct Footer {
    reserved: u32,
    offset: u32,
}

const DESCRIPTOR_SIZE: usize = size_of::<PacketDescriptor>();
const FOOTER_SIZE: usize = size_of::<Footer>();

/// Round `n` up to a multiple of 8.
const fn align8(n: usize) -> usize {
    (n + 7) & !7
}

/// Backing memory for one ring buffer.
///
/// Implementors expose a control page (accessed as `AtomicU32`) and a
/// power-of-two-sized data area that supports byte-granular reads and
/// writes. The trait is intentionally minimal so we can share the send
/// / recv state machine between:
///   * a test-friendly [`FlatRingMem`] backed by a boxed byte slice, and
///   * a UEFI implementation over guest-physical pages (added later).
///
/// # Contract
///
/// * The data area size in bytes is a power of two and a multiple of 8.
/// * `read_at` / `write_at` treat the data area as a wrap-around ring:
///   offsets outside `[0, data_len)` panic. Callers are responsible for
///   wrapping.
/// * Byte reads and writes are not synchronised with the peer — the
///   `write_index` / `read_index` publish/subscribe is what enforces
///   memory ordering.
pub trait RingMem {
    /// Control words. Must be at least [`CONTROL_WORD_COUNT`] entries.
    fn control(&self) -> &[AtomicU32];
    /// Size of the data area in bytes.
    fn data_len(&self) -> usize;
    /// Copy `data.len()` bytes from `off` into `data`. Offset must be
    /// in `[0, data_len)`.
    fn read_at(&self, off: usize, data: &mut [u8]);
    /// Copy `data` into the ring at `off`. Offset must be in
    /// `[0, data_len)`.
    fn write_at(&self, off: usize, data: &[u8]);
}

/// A boxed-slice backed [`RingMem`] used for host tests. Not intended
/// for the real UEFI code path — that will supply its own [`RingMem`]
/// that reads / writes guest-physical memory directly.
pub struct FlatRingMem {
    control: Box<[AtomicU32]>,
    data: Box<[AtomicU8]>,
}

impl FlatRingMem {
    /// Create a ring with `data_len` bytes of data area. Must be a
    /// power of two and a multiple of 8.
    pub fn new(data_len: usize) -> Self {
        assert!(data_len.is_power_of_two() && data_len >= 8);
        let control: Box<[AtomicU32]> =
            (0..CONTROL_WORD_COUNT).map(|_| AtomicU32::new(0)).collect();
        let data: Box<[AtomicU8]> = (0..data_len).map(|_| AtomicU8::new(0)).collect();
        Self { control, data }
    }
}

impl RingMem for FlatRingMem {
    fn control(&self) -> &[AtomicU32] {
        &self.control
    }

    fn data_len(&self) -> usize {
        self.data.len()
    }

    fn read_at(&self, off: usize, data: &mut [u8]) {
        assert!(off < self.data.len());
        let mask = self.data.len() - 1;
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = self.data[(off + i) & mask].load(Ordering::Relaxed);
        }
    }

    fn write_at(&self, off: usize, data: &[u8]) {
        assert!(off < self.data.len());
        let mask = self.data.len() - 1;
        for (i, byte) in data.iter().enumerate() {
            self.data[(off + i) & mask].store(*byte, Ordering::Relaxed);
        }
    }
}

/// Owning ring memory backed by an aligned byte buffer. Suitable for
/// both host tests and the eventual UEFI implementation once the caller
/// supplies a page-aligned allocation.
pub struct OwnedRingMem {
    control: Box<[AtomicU32]>,
    data: Box<[AtomicU8]>,
}

impl OwnedRingMem {
    /// Allocate a ring with `data_pages` pages of data (each 4096 bytes).
    /// `data_pages` must be a power of two.
    pub fn new(data_pages: usize) -> Self {
        assert!(data_pages.is_power_of_two() && data_pages > 0);
        let data_len = data_pages * CONTROL_PAGE_SIZE;
        let control = (0..CONTROL_WORD_COUNT).map(|_| AtomicU32::new(0)).collect();
        let data = vec![0u8; data_len].into_iter().map(AtomicU8::new).collect();
        Self { control, data }
    }
}

impl RingMem for OwnedRingMem {
    fn control(&self) -> &[AtomicU32] {
        &self.control
    }

    fn data_len(&self) -> usize {
        self.data.len()
    }

    fn read_at(&self, off: usize, data: &mut [u8]) {
        assert!(off < self.data.len());
        let mask = self.data.len() - 1;
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = self.data[(off + i) & mask].load(Ordering::Relaxed);
        }
    }

    fn write_at(&self, off: usize, data: &[u8]) {
        assert!(off < self.data.len());
        let mask = self.data.len() - 1;
        for (i, byte) in data.iter().enumerate() {
            self.data[(off + i) & mask].store(*byte, Ordering::Relaxed);
        }
    }
}

/// A [`RingMem`] backed by two raw pointers into identity-mapped
/// guest-physical memory.
///
/// Unlike [`FlatRingMem`] / [`OwnedRingMem`] this does not own the
/// pages — the caller must keep them alive (typically by allocating
/// them from a leaky global allocator) and must ensure the layout
/// matches the VMBus wire format: the `control` pointer references a
/// 4 KiB control page whose first `CONTROL_WORD_COUNT` `u32` slots
/// hold the ring indices, and `data` points at `data_len` bytes of
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
        }
    }
}

impl RingMem for RawRingMem {
    fn control(&self) -> &[AtomicU32] {
        // SAFETY: caller of `new` guaranteed the control pointer is
        // valid for `CONTROL_WORD_COUNT` `AtomicU32`s and outlives us.
        #[expect(unsafe_code, reason = "materialise slice over control page")]
        unsafe {
            core::slice::from_raw_parts(self.control, CONTROL_WORD_COUNT)
        }
    }

    fn data_len(&self) -> usize {
        self.data_len
    }

    fn read_at(&self, off: usize, data: &mut [u8]) {
        assert!(off < self.data_len);
        let mask = self.data_len - 1;
        for (i, byte) in data.iter_mut().enumerate() {
            // SAFETY: caller of `new` guaranteed data is valid for
            // `data_len` bytes; masking keeps the index in range.
            #[expect(unsafe_code, reason = "raw ring data read")]
            unsafe {
                *byte = (*self.data.add((off + i) & mask)).load(Ordering::Relaxed);
            }
        }
    }

    fn write_at(&self, off: usize, data: &[u8]) {
        assert!(off < self.data_len);
        let mask = self.data_len - 1;
        for (i, byte) in data.iter().enumerate() {
            // SAFETY: as above.
            #[expect(unsafe_code, reason = "raw ring data write")]
            unsafe {
                (*self.data.add((off + i) & mask)).store(*byte, Ordering::Relaxed);
            }
        }
    }
}

/// Common accessor helpers shared by [`SendRing`] and [`RecvRing`].
fn ctrl_in<M: RingMem>(m: &M) -> &AtomicU32 {
    &m.control()[IDX_IN]
}
fn ctrl_out<M: RingMem>(m: &M) -> &AtomicU32 {
    &m.control()[IDX_OUT]
}
fn ctrl_interrupt_mask<M: RingMem>(m: &M) -> &AtomicU32 {
    &m.control()[IDX_INTERRUPT_MASK]
}
fn ctrl_pending_send<M: RingMem>(m: &M) -> &AtomicU32 {
    &m.control()[IDX_PENDING_SEND_SZ]
}

/// Write `bytes` into `mem` at `off`, wrapping at the ring boundary.
fn write_wrapping<M: RingMem>(mem: &M, off: usize, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let len = mem.data_len();
    let start = off & (len - 1);
    if start + bytes.len() <= len {
        mem.write_at(start, bytes);
    } else {
        let first = len - start;
        mem.write_at(start, &bytes[..first]);
        mem.write_at(0, &bytes[first..]);
    }
}

/// Read `bytes` from `mem` at `off`, wrapping at the ring boundary.
fn read_wrapping<M: RingMem>(mem: &M, off: usize, bytes: &mut [u8]) {
    if bytes.is_empty() {
        return;
    }
    let len = mem.data_len();
    let start = off & (len - 1);
    if start + bytes.len() <= len {
        mem.read_at(start, bytes);
    } else {
        let first = len - start;
        let (a, b) = bytes.split_at_mut(first);
        mem.read_at(start, a);
        mem.read_at(0, b);
    }
}

/// Number of free bytes available for the writer.
///
/// One slot is reserved so `write_idx == read_idx` unambiguously means
/// empty (Linux ring_buffer.c does the same).
fn available_free(write_idx: u32, read_idx: u32, ring_len: u32) -> u32 {
    if write_idx >= read_idx {
        ring_len - (write_idx - read_idx) - 8
    } else {
        read_idx - write_idx - 8
    }
}

/// Number of bytes the reader can pull from the ring.
fn available_data(write_idx: u32, read_idx: u32, ring_len: u32) -> u32 {
    if write_idx >= read_idx {
        write_idx - read_idx
    } else {
        ring_len - (read_idx - write_idx)
    }
}

/// Writer half of a ring buffer.
pub struct SendRing<M: RingMem> {
    mem: M,
}

impl<M: RingMem> SendRing<M> {
    /// Construct a new send ring over `mem`.
    pub fn new(mem: M) -> Self {
        Self { mem }
    }

    /// Backing memory.
    pub fn mem(&self) -> &M {
        &self.mem
    }

    /// Post an inband packet with `payload`. Returns `true` if the
    /// caller should signal the peer (i.e. the ring transitioned from
    /// empty to non-empty and the peer has interrupts unmasked).
    ///
    /// See `PACKET_TYPE_IN_BAND` (0x6) in openvmm's `vmbus_ring`.
    pub fn write_inband(
        &self,
        payload: &[u8],
        flags: PacketFlags,
        transaction_id: u64,
    ) -> Result<bool> {
        self.write_packet(
            PacketType::VM_PKT_DATA_INBAND,
            &[],
            payload,
            flags,
            transaction_id,
        )
    }

    /// Post a completion packet (`VM_PKT_COMP`, 0xB) referencing the
    /// original `transaction_id`.
    pub fn write_completion(&self, payload: &[u8], transaction_id: u64) -> Result<bool> {
        self.write_packet(
            PacketType::VM_PKT_COMP,
            &[],
            payload,
            PacketFlags::new(),
            transaction_id,
        )
    }

    /// Post a packet of arbitrary type. `ext_header` is placed between
    /// the descriptor and payload (used e.g. for GPA-direct headers).
    /// Returns whether the caller should signal.
    pub fn write_packet(
        &self,
        packet_type: PacketType,
        ext_header: &[u8],
        payload: &[u8],
        flags: PacketFlags,
        transaction_id: u64,
    ) -> Result<bool> {
        let ring_len = self.mem.data_len() as u32;
        // ext_header + payload must be padded to 8. length8 / data_offset8
        // are total 64-bit words including descriptor + footer.
        let ext_hdr_aligned = align8(ext_header.len());
        let payload_aligned = align8(payload.len());
        let data_offset_bytes = DESCRIPTOR_SIZE + ext_hdr_aligned;
        let total_bytes = data_offset_bytes + payload_aligned + FOOTER_SIZE;
        if total_bytes > u16::MAX as usize * 8 {
            return Err(Error::Parse {
                ty: None,
                reason: "packet too large",
            });
        }

        // Snapshot ring pointers. `Acquire` on `out` synchronises with
        // the reader's `Release` publish of `read_index`.
        let write_idx = ctrl_in(&self.mem).load(Ordering::Relaxed);
        let read_idx = ctrl_out(&self.mem).load(Ordering::Acquire);
        let free = available_free(write_idx, read_idx, ring_len) as usize;
        if free < total_bytes {
            return Err(Error::RingFull);
        }

        // Descriptor.
        let desc = PacketDescriptor {
            packet_type,
            flags,
            data_offset8: (data_offset_bytes / 8) as u16,
            length8: (total_bytes / 8) as u16,
            transaction_id,
        };
        let mut cursor = write_idx as usize;
        write_wrapping(&self.mem, cursor, desc.as_bytes());
        cursor += DESCRIPTOR_SIZE;

        // Optional extended header (padded to 8).
        if !ext_header.is_empty() {
            write_wrapping(&self.mem, cursor, ext_header);
            let pad = ext_hdr_aligned - ext_header.len();
            if pad != 0 {
                write_wrapping(&self.mem, cursor + ext_header.len(), &[0u8; 8][..pad]);
            }
            cursor += ext_hdr_aligned;
        }

        // Payload (with zero-padding to 8).
        if !payload.is_empty() {
            write_wrapping(&self.mem, cursor, payload);
            let pad = payload_aligned - payload.len();
            if pad != 0 {
                write_wrapping(&self.mem, cursor + payload.len(), &[0u8; 8][..pad]);
            }
            cursor += payload_aligned;
        }

        // Footer.
        let footer = Footer {
            reserved: 0,
            offset: write_idx,
        };
        write_wrapping(&self.mem, cursor, footer.as_bytes());

        // Publish the new write_index. `Release` synchronises with the
        // reader's `Acquire` load.
        let new_write_idx = (write_idx + total_bytes as u32) & (ring_len - 1);
        ctrl_in(&self.mem).store(new_write_idx, Ordering::Release);

        // Signal decision: only on empty→non-empty, and only when the
        // peer hasn't masked interrupts (`interrupt_mask == 0`).
        let was_empty = write_idx == read_idx;
        let peer_wants_signal = ctrl_interrupt_mask(&self.mem).load(Ordering::Acquire) == 0;
        Ok(was_empty && peer_wants_signal)
    }
}

/// Reader half of a ring buffer.
pub struct RecvRing<M: RingMem> {
    mem: M,
}

/// A packet returned by [`RecvRing::read`].
pub struct RecvPacket<'a> {
    /// Descriptor as it appears on the wire.
    pub descriptor: PacketDescriptor,
    /// Payload bytes copied out of the ring, without the descriptor,
    /// extended header, or footer, and stripped of trailing pad.
    pub payload: &'a [u8],
    /// Length of the extended header in bytes (used by GPA-direct /
    /// transfer-page packets).
    pub ext_header_len: usize,
}

impl<M: RingMem> RecvRing<M> {
    /// Construct a new recv ring over `mem`.
    pub fn new(mem: M) -> Self {
        Self { mem }
    }

    /// Backing memory.
    pub fn mem(&self) -> &M {
        &self.mem
    }

    /// Number of bytes available to read right now.
    pub fn available(&self) -> u32 {
        let write_idx = ctrl_in(&self.mem).load(Ordering::Acquire);
        let read_idx = ctrl_out(&self.mem).load(Ordering::Relaxed);
        available_data(write_idx, read_idx, self.mem.data_len() as u32)
    }

    /// Read one packet into `buf`. Returns `Err(Error::RingEmpty)` if
    /// no packet is available.
    ///
    /// `buf` must be at least `descriptor.length8 * 8 - FOOTER_SIZE`
    /// bytes long to hold the ext-header + payload; on a smaller buffer
    /// the read fails with [`Error::Parse`] and the packet is left
    /// pending. On success `read_index` is advanced past the packet.
    pub fn read<'a>(&self, buf: &'a mut [u8]) -> Result<RecvPacket<'a>> {
        let ring_len = self.mem.data_len() as u32;
        let write_idx = ctrl_in(&self.mem).load(Ordering::Acquire);
        let read_idx = ctrl_out(&self.mem).load(Ordering::Relaxed);
        if write_idx == read_idx {
            return Err(Error::RingEmpty);
        }

        // Read the descriptor.
        let mut desc_bytes = [0u8; DESCRIPTOR_SIZE];
        read_wrapping(&self.mem, read_idx as usize, &mut desc_bytes);
        let (descriptor, _) =
            PacketDescriptor::read_from_prefix(&desc_bytes).map_err(|_| Error::Parse {
                ty: None,
                reason: "descriptor cast failed",
            })?;

        let length_bytes = descriptor.length8 as usize * 8;
        let data_offset_bytes = descriptor.data_offset8 as usize * 8;
        if length_bytes < data_offset_bytes + FOOTER_SIZE
            || data_offset_bytes < DESCRIPTOR_SIZE
            || length_bytes > available_data(write_idx, read_idx, ring_len) as usize
        {
            return Err(Error::Parse {
                ty: None,
                reason: "descriptor length out of range",
            });
        }
        let payload_bytes = length_bytes - data_offset_bytes - FOOTER_SIZE;
        let ext_header_len = data_offset_bytes - DESCRIPTOR_SIZE;
        let needed = ext_header_len + payload_bytes;
        if buf.len() < needed {
            return Err(Error::Parse {
                ty: None,
                reason: "recv buffer smaller than packet payload",
            });
        }

        // Read the extended header + payload region into `buf`.
        let ext_off = read_idx as usize + DESCRIPTOR_SIZE;
        read_wrapping(&self.mem, ext_off, &mut buf[..needed]);

        // Advance read_index past the whole packet.
        let new_read_idx = (read_idx + length_bytes as u32) & (ring_len - 1);
        ctrl_out(&self.mem).store(new_read_idx, Ordering::Release);

        Ok(RecvPacket {
            descriptor,
            payload: &buf[ext_header_len..needed],
            ext_header_len,
        })
    }

    /// Mask host→guest signalling.
    pub fn set_interrupt_mask(&self, masked: bool) {
        ctrl_interrupt_mask(&self.mem).store(masked as u32, Ordering::Release);
    }

    /// Ask the peer to signal us when at least `size` free bytes are
    /// available in the ring. Setting to 0 disables the hint.
    pub fn set_pending_send_size(&self, size: u32) {
        ctrl_pending_send(&self.mem).store(size, Ordering::Release);
    }
}
