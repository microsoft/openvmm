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
//! transition when `interrupt_mask == 0`. Signalling on every
//! packet risks Hyper-V's DoS throttling; the peer clears
//! `interrupt_mask` to explicitly ask for the wake.
//!
//! # Usage
//!
//! Rings are single-producer / single-consumer, always paired
//! `SendRing<M>` + `RecvRing<M>` per direction, both wrapping a shared
//! memory abstraction:
//!
//! * [`RawRingMem`] — for real GPA-mapped rings (UEFI target).
//! * [`OwnedRingMem`] — for host tests. Allocates a boxed buffer.
//! * [`FlatRingMem`] — for pure computation in tests.
//!
//! ## Writing a packet (guest → host)
//!
//! ```ignore
//! use vmbus_guest::ring::{SendRing, PacketFlags};
//!
//! let mut flags = PacketFlags::new();
//! flags.set_request_completion(true);
//! let need_signal = send.write_inband(payload, flags, /*tid=*/ 42)?;
//! // `need_signal` (returned by every `write_*` method) is `true`
//! // exactly when this write crossed the empty→non-empty transition
//! // AND the peer hasn't masked interrupts. Signal only when it's
//! // true — unconditional signalling wakes the host once per packet
//! // and risks Hyper-V's DoS throttling (see the top-level note on
//! // the writer signalling only on empty→non-empty).
//! if need_signal {
//!     channel.signal(&mut ctx)?;
//! }
//! # Ok::<_, vmbus_guest::Error>(())
//! ```
//!
//! For a GPA-direct external buffer (used by netvsp for RNDIS
//! payloads), use [`SendRing::write_gpa_direct`] instead — it packs
//! the descriptor + [`crate::protocol::GpaDirectHeader`] +
//! [`crate::protocol::GpaRange`] + PFN list before the NVSP payload.
//!
//! ## Reading a packet (host → guest)
//!
//! ```ignore
//! use vmbus_guest::Error;
//! let mut buf = [0u8; 4096];
//! loop {
//!     match recv.read(&mut buf) {
//!         Ok(pkt) => { /* dispatch on pkt.descriptor.packet_type */ }
//!         Err(Error::RingEmpty) => break,
//!         Err(e) => return Err(e),
//!     }
//! }
//! // After draining a batch, tell the host whether it needs to be
//! // woken (only fires on the pending_send_sz threshold crossing).
//! if recv.drain_signal_decision(bytes_read) == SignalDecision::Signal {
//!     channel.signal(&mut ctx)?;
//! }
//! ```
//!
//! ## Back-pressure (writer blocked)
//!
//! When [`SendRing::write_packet`] returns
//! [`crate::Error::RingFull`], the ring has already published a
//! non-zero `pending_send_sz` hint so the reader will kick us once it
//! frees enough room. Bounded retry / caller-supplied yielding is
//! the correct response; do not busy-loop the write.
//!
//! # Concurrency
//!
//! Rings are single-threaded on both sides. On weakly-ordered targets
//! (aarch64 UEFI) the implementation uses `SeqCst` on both the
//! writer's `write_idx` publish + `read_idx` reload and the reader's
//! `read_idx` publish + `pending_send_sz` reload to close the Dekker
//! rendezvous that decides when a signal is needed.

use crate::Error;
use crate::Result;
use crate::protocol::PacketDescriptor;
use crate::protocol::PacketType;
use alloc::boxed::Box;
use alloc::vec;
use core::marker::PhantomData;
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
fn ctrl_feature_bits<M: RingMem>(m: &M) -> &AtomicU32 {
    &m.control()[IDX_FEATURE_BITS]
}

/// Feature bit 0 in `feature_bits`. When set by the ring's **writer**,
/// the writer promises to observe the `pending_send_sz` protocol
/// (i.e. it will kick the reader when free space crosses the pending
/// threshold). Openvmm and Linux both check this bit on the ring
/// they're reading, so the guest must set it on its **SendRing** at
/// init; the host sets it on the guest's **RecvRing**.
///
/// Matches `vmbus_ring::FEATURE_SUPPORTS_PENDING_SEND_SIZE = 1`.
pub const FEATURE_SUPPORTS_PENDING_SEND_SIZE: u32 = 0x1;

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
///
/// Concurrency-unsafe on purpose: a ring is a single-producer /
/// single-consumer channel, and the writer side owns the
/// `pending_send_sz` protocol on the control page. The
/// `PhantomData<*const ()>` marker makes `SendRing` `!Send + !Sync`
/// so callers can't accidentally share a writer across threads.
pub struct SendRing<M: RingMem> {
    mem: M,
    _not_send_sync: PhantomData<*const ()>,
}

impl<M: RingMem> SendRing<M> {
    /// Construct a new send ring over `mem`.
    ///
    /// Advertises `FEATURE_SUPPORTS_PENDING_SEND_SIZE` on the send
    /// ring's control page — the guest is the writer of this ring,
    /// and the writer owns the `feature_bits` slot per
    /// `vmbus_ring::OutgoingRing::new` in openvmm. Openvmm and
    /// Linux both check this bit on the ring they're reading before
    /// honouring any `pending_send_sz` we might post.
    ///
    /// Also zeros `pending_send_sz` to a known state.
    pub fn new(mem: M) -> Self {
        ctrl_feature_bits(&mem).store(FEATURE_SUPPORTS_PENDING_SEND_SIZE, Ordering::Relaxed);
        ctrl_pending_send(&mem).store(0, Ordering::Relaxed);
        Self {
            mem,
            _not_send_sync: PhantomData,
        }
    }

    /// Set the pending-send-size hint on our SendRing's control page.
    ///
    /// The peer (host reader) inspects this after draining and, on a
    /// transition from "not enough space" → "enough space", signals
    /// us. `size` is the number of free bytes we need before we can
    /// make progress. `size == 0` clears the hint. Uses `SeqCst` to
    /// keep ordering with the `read_idx` load we perform on the
    /// retry path in `write_packet`.
    pub fn set_pending_send_size(&self, size: u32) {
        ctrl_pending_send(&self.mem).store(size, Ordering::SeqCst);
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

    /// Post a `VM_PKT_DATA_USING_GPA_DIRECT` (type 0x9) packet with
    /// a single-range GPA-direct extended header referencing an
    /// external, contiguous buffer via its guest PFNs.
    ///
    /// Wire layout after descriptor:
    /// ```text
    /// GpaDirectHeader { reserved: 0, range_count: 1 }
    /// GpaRange { byte_count, byte_offset }
    /// u64 pfns[]
    /// ```
    /// then the (optional) `payload` bytes, then footer.
    ///
    /// * `pfns` — page frame numbers of the external data buffer,
    ///   in order. Must not be empty.
    /// * `byte_offset` — byte offset into the first PFN's page where
    ///   the data starts (typically 0 for page-aligned buffers).
    /// * `byte_count` — total byte length of the external data. Must
    ///   be `<= pfns.len() * 4096 - byte_offset`.
    /// * `payload` — additional inline payload (typically the NVSP
    ///   `Nvsp1MsgSendRndisPacket` header). Empty payload is fine.
    ///
    /// Called with `flags.set_request_completion(true)` when a
    /// completion is expected.
    pub fn write_gpa_direct(
        &self,
        pfns: &[u64],
        byte_offset: u32,
        byte_count: u32,
        payload: &[u8],
        flags: PacketFlags,
        transaction_id: u64,
    ) -> Result<bool> {
        if pfns.is_empty() {
            return Err(Error::Parse {
                ty: None,
                reason: "write_gpa_direct requires >= 1 PFN",
            });
        }
        // Guard against byte_offset >= the covered region so the
        // subtraction below can't underflow into a huge u64 and
        // silently pass a caller that's out of range.
        let region = (pfns.len() as u64).saturating_mul(4096);
        if byte_offset as u64 >= region {
            return Err(Error::Parse {
                ty: None,
                reason: "write_gpa_direct byte_offset exceeds PFN range",
            });
        }
        let expected_bytes = region - byte_offset as u64;
        if (byte_count as u64) > expected_bytes {
            return Err(Error::Parse {
                ty: None,
                reason: "write_gpa_direct byte_count exceeds PFN range",
            });
        }

        // Build the extended header on the stack — max reasonable
        // size is `range_count=1, PFN count < 4` for our RNDIS use
        // (single-page control message). Cap at 32 PFNs = ~264 bytes.
        //
        // Header layout:
        //   GpaDirectHeader (8B) + GpaRange (8B) + PFNs (8B × N)
        const MAX_PFNS: usize = 32;
        if pfns.len() > MAX_PFNS {
            return Err(Error::Parse {
                ty: None,
                reason: "write_gpa_direct pfn list too long for stack buffer",
            });
        }
        let mut ext_buf = [0u8; 16 + 8 * MAX_PFNS];
        let hdr = crate::protocol::GpaDirectHeader {
            reserved: 0,
            range_count: 1,
        };
        let rng = crate::protocol::GpaRange {
            byte_count,
            byte_offset,
        };
        ext_buf[..8].copy_from_slice(hdr.as_bytes());
        ext_buf[8..16].copy_from_slice(rng.as_bytes());
        for (i, &pfn) in pfns.iter().enumerate() {
            let off = 16 + i * 8;
            ext_buf[off..off + 8].copy_from_slice(&pfn.to_le_bytes());
        }
        let ext_len = 16 + pfns.len() * 8;
        self.write_packet(
            PacketType::VM_PKT_DATA_USING_GPA_DIRECT,
            &ext_buf[..ext_len],
            payload,
            flags,
            transaction_id,
        )
    }

    /// Post a packet of arbitrary type. `ext_header` is placed between
    /// the descriptor and payload (used e.g. for GPA-direct headers).
    ///
    /// # Return value
    ///
    /// Returns `Ok(true)` exactly when this write crossed the ring
    /// from empty to non-empty **and** the peer hasn't masked
    /// interrupts (`interrupt_mask == 0`). Callers should invoke
    /// [`crate::channel::Channel::signal`] **only** on that transition
    /// — signalling on every packet risks Hyper-V's DoS throttling
    /// (see the module-level note on empty→non-empty signalling).
    /// Returns `Ok(false)` on a successful write that doesn't cross
    /// the transition (host already has data queued or has masked
    /// interrupts).
    pub fn write_packet(
        &self,
        packet_type: PacketType,
        ext_header: &[u8],
        payload: &[u8],
        flags: PacketFlags,
        transaction_id: u64,
    ) -> Result<bool> {
        let ring_len = self.mem.data_len() as u32;
        // `msg_len` = descriptor + ext_header + payload (all padded to
        // 8). This is what goes into `length8`. `total_ring_len` also
        // includes the 8-byte footer and is what we advance
        // `write_idx` by. Must match openvmm/Windows/Linux wire
        // convention — see `vmbus_ring::OutgoingRing::write` in
        // openvmm:
        //   length8 = msg_len / 8    (EXCLUDES footer)
        //   ring advances by msg_len + FOOTER_SIZE
        // Getting this wrong makes every packet mis-parseable by the
        // host.
        let ext_hdr_aligned = align8(ext_header.len());
        let payload_aligned = align8(payload.len());
        let data_offset_bytes = DESCRIPTOR_SIZE + ext_hdr_aligned;
        let msg_len = data_offset_bytes + payload_aligned;
        let total_ring_len = msg_len + FOOTER_SIZE;
        if msg_len > u16::MAX as usize * 8 {
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
        if free < total_ring_len {
            // Not enough room. Publish the pending_send_sz hint on
            // our SendRing so the reader knows how much space we
            // need before signalling us. Then reload read_idx
            // (SeqCst) and recheck; a concurrent reader may have
            // drained between the initial load above and our store
            // below. Without the recheck we could lose the wakeup
            // and deadlock (writer waits for signal that reader
            // won't send because pending_send_sz wasn't visible
            // yet when it drained).
            ctrl_pending_send(&self.mem).store(total_ring_len as u32, Ordering::SeqCst);
            let read_idx_reload = ctrl_out(&self.mem).load(Ordering::SeqCst);
            let free_reload = available_free(write_idx, read_idx_reload, ring_len) as usize;
            if free_reload < total_ring_len {
                // Still full. Leave pending_send_sz set — the reader
                // will kick us when it frees the room.
                return Err(Error::RingFull);
            }
            // Space appeared after our store. Clear the hint (we
            // don't need a signal) and fall through to the write.
            ctrl_pending_send(&self.mem).store(0, Ordering::SeqCst);
        }

        // Descriptor.
        let desc = PacketDescriptor {
            packet_type,
            flags,
            data_offset8: (data_offset_bytes / 8) as u16,
            length8: (msg_len / 8) as u16,
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

        // Publish the new write_index with SeqCst. This is required
        // to correctly race with the reader's SeqCst read_index
        // store in `RecvRing::read`: the SeqCst pair guarantees that
        // when both threads reload the peer's index after their own
        // publish, at least one observes the other's fresh value.
        // Without this, the "was the reader idle at publish time"
        // test below can miss a wakeup and deadlock the guest→host
        // path (matches openvmm `OutgoingRing::commit_write` in
        // `vmbus_ring::lib.rs`, and the memory-barrier + reload
        // pattern in Linux's `hv_signal_on_write`).
        let old_write_idx = write_idx;
        let new_write_idx = (write_idx + total_ring_len as u32) & (ring_len - 1);
        ctrl_in(&self.mem).store(new_write_idx, Ordering::SeqCst);

        // Signal decision: after publishing our new write_idx,
        // reload read_idx with SeqCst. The reader was idle at the
        // moment we published iff it has caught up to the write_idx
        // we had **before** this write — i.e. `read_idx_after ==
        // old_write_idx`. Comparing against the pre-load snapshot
        // (as we used to do) misses the case where the reader
        // drained everything between the pre-load and our publish
        // and then parked.
        let read_idx_after = ctrl_out(&self.mem).load(Ordering::SeqCst);
        let was_empty = read_idx_after == old_write_idx;
        let peer_wants_signal = ctrl_interrupt_mask(&self.mem).load(Ordering::SeqCst) == 0;
        Ok(was_empty && peer_wants_signal)
    }
}

/// Reader half of a ring buffer.
///
/// Concurrency-unsafe on purpose: a ring is a single-producer /
/// single-consumer channel, and the reader side owns the read-index
/// publish that pairs with the writer's `pending_send_sz` protocol.
/// The `PhantomData<*const ()>` marker makes `RecvRing` `!Send +
/// !Sync` so callers can't accidentally share a reader across
/// threads.
pub struct RecvRing<M: RingMem> {
    mem: M,
    _not_send_sync: PhantomData<*const ()>,
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
        Self {
            mem,
            _not_send_sync: PhantomData,
        }
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

        // Wire semantics: `length8` is msg_len/8 EXCLUDING the
        // 8-byte footer (matches openvmm `vmbus_ring::parse_packet`
        // and Windows). The reader must advance by
        // `msg_len + FOOTER_SIZE`.
        let msg_len = descriptor.length8 as usize * 8;
        let data_offset_bytes = descriptor.data_offset8 as usize * 8;
        let total_ring_len = msg_len + FOOTER_SIZE;
        if msg_len < data_offset_bytes
            || data_offset_bytes < DESCRIPTOR_SIZE
            || total_ring_len > available_data(write_idx, read_idx, ring_len) as usize
        {
            return Err(Error::Parse {
                ty: None,
                reason: "descriptor length out of range",
            });
        }
        let payload_bytes = msg_len - data_offset_bytes;
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

        // Advance read_index past the whole packet (msg_len + footer).
        // SeqCst is required to correctly rendezvous with the peer
        // writer's `pending_send_sz` protocol: the writer does
        // `pending_send_sz.store(SeqCst); read_idx.load(SeqCst)`; the
        // reader must mirror with a SeqCst store on read_idx (and a
        // SeqCst load of pending_send_sz in `drain_signal_decision`)
        // for the "at least one side observes the other's store"
        // guarantee to hold on weakly-ordered targets (aarch64 UEFI).
        // openvmm's `IncomingRing::commit_read` uses SeqCst here for
        // the same reason.
        let new_read_idx = (read_idx + total_ring_len as u32) & (ring_len - 1);
        ctrl_out(&self.mem).store(new_read_idx, Ordering::SeqCst);

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

    /// Whether the ring's **writer** (the host, for a RecvRing) has
    /// advertised support for the `pending_send_sz` protocol. When
    /// this is false, we should not perform the reader-side signal
    /// decision — the host won't be listening.
    pub fn supports_pending_send_size(&self) -> bool {
        let bits = ctrl_feature_bits(&self.mem).load(Ordering::Relaxed);
        (bits & FEATURE_SUPPORTS_PENDING_SEND_SIZE) != 0
    }

    /// Read the writer's current pending-send-size hint. Non-zero
    /// means the writer is blocked waiting for at least this many
    /// free bytes.
    pub fn pending_send_size(&self) -> u32 {
        ctrl_pending_send(&self.mem).load(Ordering::SeqCst)
    }
}

/// Decision returned by [`RecvRing::drain_signal_decision`] after
/// draining packets from the RECV ring.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SignalDecision {
    /// The peer (host) is blocked writing and just crossed from
    /// "not enough space" → "enough space". The caller should invoke
    /// `Channel::signal(ctx)` to wake it.
    Signal,
    /// No signal is needed: the writer isn't blocked, doesn't
    /// support the protocol, or hasn't crossed the transition.
    NoSignal,
}

impl<M: RingMem> RecvRing<M> {
    /// Compute the signal decision for a batch of reads that
    /// advanced `read_index` by `bytes_read`. This is the
    /// reader-side half of the `pending_send_sz` protocol: the
    /// peer writer parks itself and posts a `pending_send_sz`
    /// hint (in bytes) when its ring is full; we drain, compute
    /// how much space is now free, and signal the writer only on
    /// the transition from "not enough free space" to "enough
    /// free space".
    ///
    /// Call this **after** all reads in a batch have completed and
    /// `read_index` has been published. Returns
    /// [`SignalDecision::Signal`] on the exact boundary crossing —
    /// signalling on every drain would risk Hyper-V's DoS throttling.
    ///
    /// # Convention
    ///
    /// `bytes_read` is the difference between the pre-drain and
    /// post-drain `read_index` (mod ring length). Callers can obtain
    /// it by snapshotting `mem().control()[IDX_OUT]` before their
    /// first read.
    ///
    /// The test corresponds to:
    /// * `old_free < pending_send_sz` **and**
    /// * `new_free >= pending_send_sz`
    ///
    /// Matches Linux's `hv_pkt_iter_close` and openvmm's
    /// `IncomingRing::commit_read_and_notify` semantics.
    pub fn drain_signal_decision(&self, bytes_read: u32) -> SignalDecision {
        if !self.supports_pending_send_size() {
            return SignalDecision::NoSignal;
        }
        let pending = self.pending_send_size();
        if pending == 0 {
            return SignalDecision::NoSignal;
        }
        let ring_len = self.mem.data_len() as u32;
        // SeqCst on both loads — the reader half of the pending_send_sz
        // Dekker rendezvous requires all four Dekker operations
        // (writer's store+load, reader's store+load) be SeqCst.
        let write_idx = ctrl_in(&self.mem).load(Ordering::SeqCst);
        let read_idx = ctrl_out(&self.mem).load(Ordering::SeqCst);
        let new_free = available_free(write_idx, read_idx, ring_len);
        // `old_free` reconstructed: before this batch of reads,
        // `read_idx` was `bytes_read` behind, so `free` was smaller
        // by the same amount.
        let old_free = new_free.saturating_sub(bytes_read);
        if old_free < pending && new_free >= pending {
            SignalDecision::Signal
        } else {
            SignalDecision::NoSignal
        }
    }
}
