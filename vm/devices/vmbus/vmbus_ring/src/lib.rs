// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module implements the low-level interface to the VmBus ring buffer. The
//! ring buffer resides in guest memory and is mapped into the host, allowing
//! efficient transfer of variable-sized packets.
//!
//! Ring buffer packets have headers called descriptors, which can specify a
//! transaction ID and metadata referring to memory outside the ring buffer.
//! Each packet is a multiple of 8 bytes.
//!
//! In practice, ring buffers always come in pairs so that packets can be both
//! sent and received. However, this module's interfaces operate on them singly.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod gparange;

pub use pipe_protocol::*;
pub use protocol::PAGE_SIZE;
pub use protocol::TransferPageRange;

use crate::gparange::GpaRange;
use guestmem::AccessError;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
use guestmem::ranges::PagedRange;
use inspect::Inspect;
use protocol::*;
use safeatomic::AtomicSliceOps;
use std::fmt::Debug;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use thiserror::Error;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

mod pipe_protocol {
    use zerocopy::FromBytes;
    use zerocopy::Immutable;
    use zerocopy::IntoBytes;
    use zerocopy::KnownLayout;

    /// Pipe channel packets are prefixed with this header to allow for
    /// non-8-multiple lengths.
    #[repr(C)]
    #[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct PipeHeader {
        pub packet_type: u32,
        pub len: u32,
    }

    /// Regular data packet.
    pub const PIPE_PACKET_TYPE_DATA: u32 = 1;
    /// Data packet that has been partially consumed, in which case the `len`
    /// field's high word is the number of bytes already read. The opposite
    /// endpoint will never write this type.
    pub const PIPE_PACKET_TYPE_PARTIAL: u32 = 2;
    /// Setup a GPA direct buffer for RDMA.
    pub const PIPE_PACKET_TYPE_SETUP_GPA_DIRECT: u32 = 3;
    /// Tear down a GPA direct buffer.
    pub const PIPE_PACKET_TYPE_TEARDOWN_GPA_DIRECT: u32 = 4;

    /// The maximum size of a pipe packet's payload.
    pub const MAXIMUM_PIPE_PACKET_SIZE: usize = 16384;
}

mod protocol {
    use crate::CONTROL_WORD_COUNT;
    use inspect::Inspect;
    use safeatomic::AtomicSliceOps;
    use std::fmt::Debug;
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::Ordering;
    use zerocopy::FromBytes;
    use zerocopy::Immutable;
    use zerocopy::IntoBytes;
    use zerocopy::KnownLayout;

    /// VmBus ring buffers are sized in multiples 4KB pages, with a 4KB control page.
    pub const PAGE_SIZE: usize = 4096;

    /// The descriptor header on every packet.
    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct PacketDescriptor {
        pub packet_type: u16,
        pub data_offset8: u16,
        pub length8: u16,
        pub flags: u16,
        pub transaction_id: u64,
    }

    /// A control page accessor.
    pub struct Control<'a>(pub &'a [AtomicU32; CONTROL_WORD_COUNT]);

    impl<'a> Control<'a> {
        pub fn from_page(page: &'a guestmem::Page) -> Option<Self> {
            let slice = page.as_atomic_slice()?[..CONTROL_WORD_COUNT]
                .try_into()
                .unwrap();
            Some(Self(slice))
        }

        pub fn inp(&self) -> &AtomicU32 {
            &self.0[0]
        }
        pub fn outp(&self) -> &AtomicU32 {
            &self.0[1]
        }
        pub fn interrupt_mask(&self) -> &AtomicU32 {
            &self.0[2]
        }
        pub fn pending_send_size(&self) -> &AtomicU32 {
            &self.0[3]
        }
        pub fn feature_bits(&self) -> &AtomicU32 {
            &self.0[16]
        }
    }

    impl Inspect for Control<'_> {
        fn inspect(&self, req: inspect::Request<'_>) {
            req.respond()
                .hex("in", self.inp().load(Ordering::Relaxed))
                .hex("out", self.outp().load(Ordering::Relaxed))
                .hex(
                    "interrupt_mask",
                    self.interrupt_mask().load(Ordering::Relaxed),
                )
                .hex(
                    "pending_send_size",
                    self.pending_send_size().load(Ordering::Relaxed),
                )
                .hex("feature_bits", self.feature_bits().load(Ordering::Relaxed));
        }
    }

    impl Debug for Control<'_> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Control")
                .field("inp", self.inp())
                .field("outp", self.outp())
                .field("interrupt_mask", self.interrupt_mask())
                .field("pending_send_size", self.pending_send_size())
                .field("feature_bits", self.feature_bits())
                .finish()
        }
    }

    /// If set, the endpoint supports sending signals when the number of free
    /// bytes in the ring reaches or exceeds `pending_send_size`.
    pub const FEATURE_SUPPORTS_PENDING_SEND_SIZE: u32 = 1;

    /// A transfer range specifying a length and offset within a transfer page
    /// set. Only used by NetVSP.
    #[repr(C)]
    #[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct TransferPageRange {
        pub byte_count: u32,
        pub byte_offset: u32,
    }

    /// The extended portion of the packet descriptor that describes a transfer
    /// page packet.
    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct TransferPageHeader {
        pub transfer_page_set_id: u16,
        pub reserved: u16, // may have garbage non-zero values
        pub range_count: u32,
    }

    /// The extended portion of the packet descriptor describing a GPA direct packet.
    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct GpaDirectHeader {
        pub reserved: u32, // may have garbage non-zero values
        pub range_count: u32,
    }

    pub const PACKET_FLAG_COMPLETION_REQUESTED: u16 = 1;

    /// The packet footer.
    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct Footer {
        pub reserved: u32,
        /// The ring offset of the packet.
        pub offset: u32,
    }
}

#[derive(Copy, Clone, Debug, Error)]
pub enum Error {
    #[error("invalid ring buffer pointer")]
    InvalidRingPointer,
    #[error("invalid message length")]
    InvalidMessageLength,
    #[error("invalid data available")]
    InvalidDataAvailable,
    #[error("ring buffer too large")]
    RingTooLarge,
    #[error("invalid ring memory")]
    InvalidRingMemory,
    #[error("invalid descriptor offset or length")]
    InvalidDescriptorLengths,
    #[error("unknown packet descriptor flags")]
    InvalidDescriptorFlags,
    #[error("unknown packet descriptor type")]
    InvalidDescriptorType,
    #[error("invalid range count for gpa direct packet")]
    InvalidDescriptorGpaDirectRangeCount,
    #[error("the interrupt mask bit was supposed to be clear but it is set")]
    InterruptsExternallyMasked,
}

#[derive(Copy, Clone, Debug, Error)]
pub enum ReadError {
    #[error("ring buffer empty")]
    Empty,
    #[error(transparent)]
    Corrupt(#[from] Error),
}

#[derive(Copy, Clone, Debug, Error)]
pub enum WriteError {
    #[error("ring buffer full")]
    Full(usize),
    #[error(transparent)]
    Corrupt(#[from] Error),
}

/// A range within a ring buffer.
#[derive(Copy, Clone, Debug)]
pub struct RingRange {
    off: u32,
    size: u32,
}

impl RingRange {
    /// The empty range.
    pub fn empty() -> Self {
        RingRange { off: 0, size: 0 }
    }

    /// Retrieves a `MemoryWrite` that allows for writing to the range.
    pub fn writer<'a, T: Ring>(&self, ring: &'a T) -> RingRangeWriter<'a, T::Memory> {
        RingRangeWriter {
            start: self.off,
            end: self.off + self.size,
            mem: ring.mem(),
        }
    }

    /// Writes the full range using aligned writes of `u64` values.
    ///
    /// # Panics
    /// Panics if the ring range is not exactly the size of `data`.
    pub fn write_aligned_full<T: Ring>(&self, ring: &T, data: &[u64]) {
        assert_eq!(self.size as usize, data.len() * 8);
        ring.mem().write_aligned(self.off as usize, data.as_bytes());
    }

    /// Retrieves a `MemoryRead` that allows for writing to the range.
    pub fn reader<'a, T: Ring>(&self, ring: &'a T) -> RingRangeReader<'a, T::Memory> {
        RingRangeReader {
            start: self.off,
            end: self.off + self.size,
            mem: ring.mem(),
        }
    }

    /// Returns the length of the range.
    pub fn len(&self) -> usize {
        self.size as usize
    }

    /// Checks if this range is empty.
    pub fn is_empty(&self) -> bool {
        self.size == 0
    }
}

/// A type implementing `MemoryRead` accessing a `RingRange`.
pub struct RingRangeReader<'a, T> {
    start: u32,
    end: u32,
    mem: &'a T,
}

impl<T: RingMem> MemoryRead for RingRangeReader<'_, T> {
    fn read(&mut self, data: &mut [u8]) -> Result<&mut Self, AccessError> {
        if self.len() < data.len() {
            return Err(AccessError::OutOfRange(self.len(), data.len()));
        }
        self.mem.read_at(self.start as usize, data);
        self.start += data.len() as u32;
        Ok(self)
    }

    fn skip(&mut self, len: usize) -> Result<&mut Self, AccessError> {
        if self.len() < len {
            return Err(AccessError::OutOfRange(self.len(), len));
        }
        self.start += len as u32;
        Ok(self)
    }

    fn len(&self) -> usize {
        (self.end - self.start) as usize
    }
}

/// A type implementing `MemoryWrite` accessing a `RingRange`.
pub struct RingRangeWriter<'a, T> {
    start: u32,
    end: u32,
    mem: &'a T,
}

impl<T: RingMem> MemoryWrite for RingRangeWriter<'_, T> {
    fn write(&mut self, data: &[u8]) -> Result<(), AccessError> {
        if self.len() < data.len() {
            return Err(AccessError::OutOfRange(self.len(), data.len()));
        }
        self.mem.write_at(self.start as usize, data);
        self.start += data.len() as u32;
        Ok(())
    }

    fn fill(&mut self, _val: u8, _len: usize) -> Result<(), AccessError> {
        unimplemented!()
    }

    fn len(&self) -> usize {
        (self.end - self.start) as usize
    }
}

/// The alternate types of incoming packets. For packets with external data,
/// includes a `RingRange` whose data is the variable portion of the packet
/// descriptor.
#[derive(Debug, Copy, Clone)]
pub enum IncomingPacketType {
    InBand,
    Completion,
    GpaDirect(u32, RingRange),
    TransferPages(u16, u32, RingRange),
}

/// An incoming packet.
#[derive(Debug)]
pub struct IncomingPacket {
    pub transaction_id: Option<u64>,
    pub typ: IncomingPacketType,
    pub payload: RingRange,
}

const PACKET_TYPE_IN_BAND: u16 = 6;
const PACKET_TYPE_TRANSFER_PAGES: u16 = 0x7;
const PACKET_TYPE_GPA_DIRECT: u16 = 0x9;
const PACKET_TYPE_COMPLETION: u16 = 0xb;

fn parse_packet<M: RingMem>(
    ring: &M,
    ring_off: u32,
    avail: u32,
) -> Result<(u32, IncomingPacket), ReadError> {
    let mut desc = PacketDescriptor::new_zeroed();
    ring.read_aligned(ring_off as usize, desc.as_mut_bytes());
    let len = desc.length8 as u32 * 8;
    if desc.length8 < desc.data_offset8 || desc.data_offset8 < 2 || avail < len {
        return Err(ReadError::Corrupt(Error::InvalidDescriptorLengths));
    }

    if (desc.flags & !PACKET_FLAG_COMPLETION_REQUESTED) != 0 {
        return Err(ReadError::Corrupt(Error::InvalidDescriptorFlags));
    }
    let transaction_id = if desc.flags & PACKET_FLAG_COMPLETION_REQUESTED != 0
        || desc.packet_type == PACKET_TYPE_COMPLETION
    {
        Some(desc.transaction_id)
    } else {
        None
    };
    let typ = match desc.packet_type {
        PACKET_TYPE_IN_BAND => IncomingPacketType::InBand,
        PACKET_TYPE_COMPLETION => IncomingPacketType::Completion,
        PACKET_TYPE_TRANSFER_PAGES => {
            let mut tph = TransferPageHeader::new_zeroed();
            ring.read_aligned(ring_off as usize + 16, tph.as_mut_bytes());
            IncomingPacketType::TransferPages(
                tph.transfer_page_set_id,
                tph.range_count,
                RingRange {
                    off: ring_off + 24,
                    size: desc.data_offset8 as u32 * 8 - 24,
                },
            )
        }
        PACKET_TYPE_GPA_DIRECT => {
            let mut gph = GpaDirectHeader::new_zeroed();
            ring.read_aligned(ring_off as usize + 16, gph.as_mut_bytes());
            if gph.range_count == 0 {
                return Err(ReadError::Corrupt(
                    Error::InvalidDescriptorGpaDirectRangeCount,
                ));
            }
            IncomingPacketType::GpaDirect(
                gph.range_count,
                RingRange {
                    off: ring_off + 24,
                    size: desc.data_offset8 as u32 * 8 - 24,
                },
            )
        }
        _ => return Err(ReadError::Corrupt(Error::InvalidDescriptorType)),
    };
    let payload = RingRange {
        off: ring_off + desc.data_offset8 as u32 * 8,
        size: (desc.length8 - desc.data_offset8) as u32 * 8,
    };
    Ok((
        len,
        IncomingPacket {
            transaction_id,
            typ,
            payload,
        },
    ))
}

/// The size of the control region in 32-bit words.
pub const CONTROL_WORD_COUNT: usize = 32;

/// A trait for memory backing a ring buffer.
pub trait RingMem: Send {
    /// Returns the control page.
    fn control(&self) -> &[AtomicU32; CONTROL_WORD_COUNT];

    /// Reads from the data portion of the ring, wrapping (once) at the end of
    /// the ring. Precondition: `addr + data.len() <= self.len() * 2`.
    fn read_at(&self, addr: usize, data: &mut [u8]);

    /// Reads from the data portion of the ring, as in [`RingMem::read_at`]. `addr` and
    /// `data.len()` must be multiples of 8.
    ///
    /// `read_at` may be faster for large or variable-sized reads.
    fn read_aligned(&self, addr: usize, data: &mut [u8]) {
        debug_assert!(addr.is_multiple_of(8));
        debug_assert!(data.len().is_multiple_of(8));
        self.read_at(addr, data)
    }

    /// Writes to the data portion of the ring, wrapping (once) at the end of
    /// the ring. Precondition: `addr + data.len() <= self.len() * 2`.
    fn write_at(&self, addr: usize, data: &[u8]);

    /// Writes to the data portion of the ring, as in [`RingMem::write_at`]. `addr` and
    /// `data.len()` must be multiples of 8.
    ///
    /// `write_at` may be faster for large or variable-sized writes.
    fn write_aligned(&self, addr: usize, data: &[u8]) {
        debug_assert!(addr.is_multiple_of(8));
        debug_assert!(data.len().is_multiple_of(8));
        self.write_at(addr, data)
    }

    /// Returns the length of the ring in bytes.
    fn len(&self) -> usize;
}

/// Implementation of `RingMem` for references. Useful for tests.
impl<T: RingMem + Sync> RingMem for &'_ T {
    fn control(&self) -> &[AtomicU32; CONTROL_WORD_COUNT] {
        (*self).control()
    }
    fn read_at(&self, addr: usize, data: &mut [u8]) {
        (*self).read_at(addr, data)
    }
    fn write_at(&self, addr: usize, data: &[u8]) {
        (*self).write_at(addr, data)
    }
    fn len(&self) -> usize {
        (*self).len()
    }

    fn read_aligned(&self, addr: usize, data: &mut [u8]) {
        (*self).read_aligned(addr, data)
    }

    fn write_aligned(&self, addr: usize, data: &[u8]) {
        (*self).write_aligned(addr, data)
    }
}

#[derive(Debug)]
pub struct SingleMappedRingMem<T>(pub T);

impl<T: AsRef<[AtomicU8]>> SingleMappedRingMem<T> {
    fn control_range(&self) -> &[AtomicU8; PAGE_SIZE] {
        self.0.as_ref()[..PAGE_SIZE].try_into().unwrap()
    }

    fn data(&self) -> &[AtomicU8] {
        &self.0.as_ref()[PAGE_SIZE..]
    }
}

impl<T: AsRef<[AtomicU8]> + Send> RingMem for SingleMappedRingMem<T> {
    fn read_at(&self, mut addr: usize, data: &mut [u8]) {
        if addr >= self.len() {
            addr -= self.len();
        }
        let this_data = self.data();
        if addr + data.len() <= self.len() {
            this_data[addr..addr + data.len()].atomic_read(data);
        } else {
            let data_len = data.len();
            let (first, last) = data.split_at_mut(self.len() - addr);
            this_data[addr..].atomic_read(first);
            this_data[..data_len - (self.len() - addr)].atomic_read(last);
        }
    }

    fn write_at(&self, mut addr: usize, data: &[u8]) {
        if addr > self.len() {
            addr -= self.len();
        }
        let this_data = self.data();
        if addr + data.len() <= self.len() {
            this_data[addr..addr + data.len()].atomic_write(data);
        } else {
            let (first, last) = data.split_at(self.len() - addr);
            this_data[addr..].atomic_write(first);
            this_data[..data.len() - (self.len() - addr)].atomic_write(last);
        }
    }

    fn control(&self) -> &[AtomicU32; CONTROL_WORD_COUNT] {
        self.control_range().as_atomic_slice().unwrap()[..CONTROL_WORD_COUNT]
            .try_into()
            .unwrap()
    }

    fn len(&self) -> usize {
        self.data().len()
    }
}

/// An implementation of `RingMem` over a flat allocation. Useful for tests.
#[derive(Clone)]
pub struct FlatRingMem {
    inner: Arc<FlatRingInner>,
}

struct FlatRingInner {
    control: [AtomicU32; CONTROL_WORD_COUNT],
    data: Vec<AtomicU8>,
}

impl FlatRingMem {
    /// Allocates a new memory.
    pub fn new(len: usize) -> Self {
        let mut data = Vec::new();
        data.resize_with(len, Default::default);
        Self {
            inner: Arc::new(FlatRingInner {
                control: [0; CONTROL_WORD_COUNT].map(Into::into),
                data,
            }),
        }
    }
}

impl Debug for FlatRingMem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlatRingMem").finish()
    }
}

impl RingMem for FlatRingMem {
    fn read_at(&self, mut addr: usize, data: &mut [u8]) {
        if addr > self.len() {
            addr -= self.len();
        }
        if addr + data.len() <= self.len() {
            self.inner.data[addr..addr + data.len()].atomic_read(data);
        } else {
            let data_len = data.len();
            let (first, last) = data.split_at_mut(self.len() - addr);
            self.inner.data[addr..].atomic_read(first);
            self.inner.data[..data_len - (self.len() - addr)].atomic_read(last);
        }
    }

    fn write_at(&self, mut addr: usize, data: &[u8]) {
        if addr > self.len() {
            addr -= self.len();
        }
        if addr + data.len() <= self.len() {
            self.inner.data[addr..addr + data.len()].atomic_write(data);
        } else {
            let (first, last) = data.split_at(self.len() - addr);
            self.inner.data[addr..].atomic_write(first);
            self.inner.data[..data.len() - (self.len() - addr)].atomic_write(last);
        }
    }

    fn control(&self) -> &[AtomicU32; CONTROL_WORD_COUNT] {
        &self.inner.control
    }

    fn len(&self) -> usize {
        self.inner.data.len()
    }
}

/// A trait for ring buffer memory divided into discontiguous pages.
pub trait PagedMemory: Send {
    /// Returns the control page.
    fn control(&self) -> &[AtomicU8; PAGE_SIZE];
    /// Returns the number of data pages.
    fn data_page_count(&self) -> usize;
    /// Returns a data page.
    ///
    /// For performance reasons, `page` may be in `0..data_page_count*2`,
    /// representing the ring logically mapped twice consecutively. The
    /// implementation should return the same page for `n` and `n +
    /// data_page_count`.
    fn data(&self, page: usize) -> &[AtomicU8; PAGE_SIZE];
}

/// An implementation of [`RingMem`] on top of discontiguous pages.
#[derive(Debug, Clone)]
pub struct PagedRingMem<T>(T);

impl<T: PagedMemory> PagedRingMem<T> {
    /// Returns a new ring memory wrapping a type implementing [`PagedMemory`].
    pub fn new(inner: T) -> Self {
        Self(inner)
    }
}

impl<T: PagedMemory> RingMem for PagedRingMem<T> {
    fn len(&self) -> usize {
        self.0.data_page_count() * PAGE_SIZE
    }

    fn read_at(&self, mut addr: usize, mut data: &mut [u8]) {
        while !data.is_empty() {
            let page = addr / PAGE_SIZE;
            let offset = addr % PAGE_SIZE;
            let offset_end = PAGE_SIZE.min(offset + data.len());
            let len = offset_end - offset;
            let (this, next) = data.split_at_mut(len);
            self.0.data(page)[offset..offset_end].atomic_read(this);
            addr += len;
            data = next;
        }
    }

    fn write_at(&self, mut addr: usize, mut data: &[u8]) {
        while !data.is_empty() {
            let page = addr / PAGE_SIZE;
            let offset = addr % PAGE_SIZE;
            let offset_end = PAGE_SIZE.min(offset + data.len());
            let len = offset_end - offset;
            let (this, next) = data.split_at(len);
            self.0.data(page)[offset..offset_end].atomic_write(this);
            addr += len;
            data = next;
        }
    }

    #[inline]
    fn read_aligned(&self, addr: usize, data: &mut [u8]) {
        debug_assert!(addr.is_multiple_of(8));
        debug_assert!(data.len().is_multiple_of(8));
        for (i, b) in data.chunks_exact_mut(8).enumerate() {
            let addr = (addr & !7) + i * 8;
            let page = addr / PAGE_SIZE;
            let offset = addr % PAGE_SIZE;
            b.copy_from_slice(
                &self.0.data(page)[offset..offset + 8]
                    .as_atomic::<AtomicU64>()
                    .unwrap()
                    .load(Ordering::Relaxed)
                    .to_ne_bytes(),
            );
        }
    }

    #[inline]
    fn write_aligned(&self, addr: usize, data: &[u8]) {
        debug_assert!(addr.is_multiple_of(8));
        debug_assert!(data.len().is_multiple_of(8));
        for (i, b) in data.chunks_exact(8).enumerate() {
            let addr = (addr & !7) + i * 8;
            let page = addr / PAGE_SIZE;
            let offset = addr % PAGE_SIZE;
            self.0.data(page)[offset..offset + 8]
                .as_atomic::<AtomicU64>()
                .unwrap()
                .store(u64::from_ne_bytes(b.try_into().unwrap()), Ordering::Relaxed);
        }
    }

    #[inline]
    fn control(&self) -> &[AtomicU32; CONTROL_WORD_COUNT] {
        self.0.control().as_atomic_slice().unwrap()[..CONTROL_WORD_COUNT]
            .try_into()
            .unwrap()
    }
}

/// Information about an outgoing packet.
#[derive(Debug)]
pub struct OutgoingPacket<'a> {
    pub transaction_id: u64,
    pub size: usize,
    pub typ: OutgoingPacketType<'a>,
}

/// The outgoing packet type variants.
#[derive(Debug, Copy, Clone)]
pub enum OutgoingPacketType<'a> {
    /// A non-transactional data packet.
    InBandNoCompletion,
    /// A transactional data packet.
    InBandWithCompletion,
    /// A completion packet.
    Completion,
    /// A GPA direct packet, which can reference memory outside the ring by address.
    ///
    /// Not supported on the host side of the ring.
    GpaDirect(&'a [PagedRange<'a>]),
    /// A transfer page packet, which can reference memory outside the ring by a
    /// buffer ID and a set of offsets into some pre-established buffer
    /// (typically a GPADL).
    ///
    /// Used by networking. Should not be used in new devices--just embed the
    /// buffer offsets in the device-specific packet payload.
    TransferPages(u16, &'a [TransferPageRange]),
}

/// Namespace type with methods to compute packet sizes, for use with
/// `set_pending_send_size`.
pub struct PacketSize(());

impl PacketSize {
    /// Computes the size of an in-band packet.
    pub const fn in_band(payload_len: usize) -> usize {
        size_of::<PacketDescriptor>() + ((payload_len + 7) & !7) + size_of::<Footer>()
    }

    /// Computes the size of a completion packet.
    pub const fn completion(payload_len: usize) -> usize {
        Self::in_band(payload_len)
    }

    // Computes the size of a gpa direct packet.
    // pub fn gpa_direct()

    /// Computes the size of a transfer page packet.
    pub const fn transfer_pages(count: usize, payload_len: usize) -> usize {
        Self::in_band(payload_len)
            + size_of::<TransferPageHeader>()
            + count * size_of::<TransferPageRange>()
    }
}

/// A trait shared by the incoming and outgoing ring buffers. Used primarily
/// with `RingRange::reader` and `RingRange::writer`.
pub trait Ring {
    /// The underlying memory type.
    type Memory: RingMem;

    /// The backing memory of the ring buffer.
    fn mem(&self) -> &Self::Memory;
}

/// The interface to the receiving endpoint of a ring buffer.
#[derive(Debug)]
pub struct IncomingRing<M: RingMem> {
    inner: InnerRing<M>,
}

impl<M: RingMem> Inspect for IncomingRing<M> {
    fn inspect(&self, req: inspect::Request<'_>) {
        self.inner.inspect(req);
    }
}

/// The current incoming ring state.
#[derive(Debug, Clone, Inspect)]
pub struct IncomingOffset {
    #[inspect(hex)]
    cached_in: u32,
    #[inspect(hex)]
    committed_out: u32,
    #[inspect(hex)]
    next_out: u32,
}

impl IncomingOffset {
    /// Reverts the removal of packets that have not yet been committed.
    pub fn revert(&mut self) {
        self.next_out = self.committed_out;
    }
}

impl<M: RingMem> Ring for IncomingRing<M> {
    type Memory = M;
    fn mem(&self) -> &Self::Memory {
        &self.inner.mem
    }
}

impl<M: RingMem> IncomingRing<M> {
    /// Returns a new incoming ring. Fails if the ring memory is not sized or
    /// aligned correctly or if the ring control data is corrupt.
    pub fn new(mem: M) -> Result<Self, Error> {
        let inner = InnerRing::new(mem)?;
        // Start with interrupts masked.
        let control = inner.control();
        control.interrupt_mask().store(1, Ordering::Relaxed);
        Ok(Self { inner })
    }

    /// Indicates whether pending send size notification is supported on
    /// the vmbus ring.
    pub fn supports_pending_send_size(&self) -> bool {
        let feature_bits = self.inner.control().feature_bits().load(Ordering::Relaxed);
        (feature_bits & FEATURE_SUPPORTS_PENDING_SEND_SIZE) != 0
    }

    /// Enables or disables the interrupt mask, declaring to the opposite
    /// endpoint that interrupts should not or should be sent for a ring
    /// empty-to-non-empty transition.
    pub fn set_interrupt_mask(&self, state: bool) {
        self.inner
            .control()
            .interrupt_mask()
            .store(state as u32, Ordering::SeqCst);
    }

    /// Verifies that interrupts are currently unmasked.
    ///
    /// This can be used to check that ring state is consistent.
    pub fn verify_interrupts_unmasked(&self) -> Result<(), Error> {
        if self
            .inner
            .control()
            .interrupt_mask()
            .load(Ordering::Relaxed)
            == 0
        {
            Ok(())
        } else {
            Err(Error::InterruptsExternallyMasked)
        }
    }

    /// Returns the current incoming offset, for passing to `read` and
    /// `commit_read`.
    pub fn incoming(&self) -> Result<IncomingOffset, Error> {
        let control = self.inner.control();
        let next_out = self
            .inner
            .validate(control.outp().load(Ordering::Relaxed))?;
        let cached_in = self.inner.validate(control.inp().load(Ordering::Relaxed))?;
        Ok(IncomingOffset {
            next_out,
            cached_in,
            committed_out: next_out,
        })
    }

    /// Returns true if there are any packets to read.
    pub fn can_read(&self, incoming: &mut IncomingOffset) -> Result<bool, Error> {
        let can_read = if incoming.next_out != incoming.cached_in {
            true
        } else {
            let inp = self
                .inner
                .validate(self.inner.control().inp().load(Ordering::Acquire))?;
            // Cache the new offset to ensure a stable result.
            incoming.cached_in = inp;
            incoming.next_out != inp
        };
        Ok(can_read)
    }

    /// Commits a series of packet reads, returning whether the opposite
    /// endpoint should be signaled.
    pub fn commit_read(&self, ptrs: &mut IncomingOffset) -> bool {
        if ptrs.committed_out == ptrs.next_out {
            return false;
        }
        let control = self.inner.control();
        control.outp().store(ptrs.next_out, Ordering::SeqCst);
        let pending_send_size = control.pending_send_size().load(Ordering::SeqCst);
        // Some implementations set the pending send size to the size of the
        // ring minus 1. The intent is that a signal arrive when the ring is
        // completely empty, but this is invalid since the maximum writable ring
        // size in the size of the ring minus 8. Mask off the low bits to work
        // around this.
        let pending_send_size = pending_send_size & !7;
        let signal = if pending_send_size != 0 {
            if let Ok(inp) = self.inner.validate(control.inp().load(Ordering::SeqCst)) {
                let old_free = self.inner.free(inp, ptrs.committed_out);
                let new_free = self.inner.free(inp, ptrs.next_out);
                old_free < pending_send_size && new_free >= pending_send_size
            } else {
                false
            }
        } else {
            false
        };
        ptrs.committed_out = ptrs.next_out;
        signal
    }

    /// Parses the next packet descriptor, returning the parsed information and
    /// a range that can be used to read the packet. The caller should commit
    /// the read with `commit_read` to free up space in the ring.
    pub fn read(&self, ptrs: &mut IncomingOffset) -> Result<IncomingPacket, ReadError> {
        let outp = ptrs.next_out;
        let mut inp = ptrs.cached_in;
        if inp == outp {
            inp = self
                .inner
                .validate(self.inner.control().inp().load(Ordering::Acquire))?;
            if inp == outp {
                return Err(ReadError::Empty);
            }
            ptrs.cached_in = inp;
        }
        let avail = self.inner.available(inp, outp);
        if avail < 16 {
            return Err(ReadError::Corrupt(Error::InvalidDataAvailable));
        }
        let (len, packet) = parse_packet(&self.inner.mem, outp, avail)?;
        ptrs.next_out = self
            .inner
            .add_pointer(outp, len + size_of::<Footer>() as u32);

        Ok(packet)
    }
}

/// The sending side of a ring buffer.
#[derive(Debug)]
pub struct OutgoingRing<M: RingMem> {
    inner: InnerRing<M>,
}

impl<M: RingMem> Inspect for OutgoingRing<M> {
    fn inspect(&self, req: inspect::Request<'_>) {
        self.inner.inspect(req);
    }
}

/// An outgoing ring offset, used to determine the position to write packets to.
#[derive(Debug, Clone, Inspect)]
pub struct OutgoingOffset {
    #[inspect(hex)]
    cached_out: u32,
    #[inspect(hex)]
    committed_in: u32,
    #[inspect(hex)]
    next_in: u32,
}

impl OutgoingOffset {
    /// Reverts the insertion of packets that have not yet been committed.
    pub fn revert(&mut self) {
        self.next_in = self.committed_in;
    }
}

impl<M: RingMem> Ring for OutgoingRing<M> {
    type Memory = M;
    fn mem(&self) -> &Self::Memory {
        &self.inner.mem
    }
}

impl<M: RingMem> OutgoingRing<M> {
    /// Returns a new outgoing ring over `mem`.
    pub fn new(mem: M) -> Result<Self, Error> {
        let inner = InnerRing::new(mem)?;
        // Report to the opposite endpoint that we will send interrupts for a
        // ring full to ring non-full transition. Feature bits are set by the
        // sending side.
        let control = inner.control();
        control
            .feature_bits()
            .store(FEATURE_SUPPORTS_PENDING_SEND_SIZE, Ordering::Relaxed);
        // Start with no interrupt requested.
        control.pending_send_size().store(0, Ordering::Relaxed);
        Ok(Self { inner })
    }

    /// Returns the current outgoing offset, for passing to `write` and
    /// ultimately `commit_write`.
    pub fn outgoing(&self) -> Result<OutgoingOffset, Error> {
        let control = self.inner.control();
        let next_in = self.inner.validate(control.inp().load(Ordering::Relaxed))?;
        let cached_out = self
            .inner
            .validate(control.outp().load(Ordering::Relaxed))?;
        Ok(OutgoingOffset {
            cached_out,
            committed_in: next_in,
            next_in,
        })
    }

    /// Sets the pending send size: the number of bytes that should be free in
    /// the ring before the opposite endpoint sends a ring-non-full signal.
    ///
    /// Fails if the packet size is larger than the ring's maximum packet size.
    pub fn set_pending_send_size(&self, len: usize) -> Result<(), Error> {
        if len > self.maximum_packet_size() {
            return Err(Error::InvalidMessageLength);
        }
        self.inner
            .control()
            .pending_send_size()
            .store((len as u32 + 7) & !7, Ordering::SeqCst);

        Ok(())
    }

    /// Returns the maximum packet size that can fit in the ring.
    pub fn maximum_packet_size(&self) -> usize {
        self.inner.len() as usize - 8
    }

    /// Returns whether a packet can fit in the ring starting at the specified
    /// offset.
    pub fn can_write(&self, ptrs: &mut OutgoingOffset, len: usize) -> Result<bool, Error> {
        let can_write = if self.inner.free(ptrs.next_in, ptrs.cached_out) as usize >= len {
            true
        } else {
            let outp = self
                .inner
                .validate(self.inner.control().outp().load(Ordering::Relaxed))?;

            // Cache the new offset to ensure a stable result.
            ptrs.cached_out = outp;
            self.inner.free(ptrs.next_in, outp) as usize >= len
        };
        Ok(can_write)
    }

    /// Commits a series of writes that ended at the specified offset, returning
    /// whether the opposite endpoint should be signaled.
    pub fn commit_write(&self, ptrs: &mut OutgoingOffset) -> bool {
        if ptrs.committed_in == ptrs.next_in {
            return false;
        }
        let inp = ptrs.next_in;

        // Update the ring offset and check if the opposite endpoint needs to be
        // signaled. This is the case only if interrupts are unmasked and the
        // ring was previously empty before this write.
        let control = self.inner.control();
        control.inp().store(inp, Ordering::SeqCst);
        let needs_interrupt = control.interrupt_mask().load(Ordering::SeqCst) == 0
            && control.outp().load(Ordering::SeqCst) == ptrs.committed_in;

        ptrs.committed_in = inp;
        needs_interrupt
    }

    /// Writes the header of the next packet and returns the ring range for the
    /// payload. The caller should write the payload, then commit the write (or
    /// multiple writes) with `commit_write`.
    ///
    /// Returns `Err(RingFull(len))` if the ring is full, where `len` is the
    /// number of bytes needed to write the requested packet.
    pub fn write(
        &self,
        ptrs: &mut OutgoingOffset,
        packet: &OutgoingPacket<'_>,
    ) -> Result<RingRange, WriteError> {
        const DESCRIPTOR_SIZE: usize = size_of::<PacketDescriptor>();
        let (packet_type, header_size, flags) = match packet.typ {
            OutgoingPacketType::InBandNoCompletion => (PACKET_TYPE_IN_BAND, DESCRIPTOR_SIZE, 0),
            OutgoingPacketType::InBandWithCompletion => (
                PACKET_TYPE_IN_BAND,
                DESCRIPTOR_SIZE,
                PACKET_FLAG_COMPLETION_REQUESTED,
            ),
            OutgoingPacketType::Completion => (PACKET_TYPE_COMPLETION, DESCRIPTOR_SIZE, 0),
            OutgoingPacketType::GpaDirect(ranges) => (
                PACKET_TYPE_GPA_DIRECT,
                DESCRIPTOR_SIZE
                    + size_of::<GpaDirectHeader>()
                    + ranges.iter().fold(0, |a, range| {
                        a + size_of::<GpaRange>() + size_of_val(range.gpns())
                    }),
                PACKET_FLAG_COMPLETION_REQUESTED,
            ),
            OutgoingPacketType::TransferPages(_, ranges) => (
                PACKET_TYPE_TRANSFER_PAGES,
                DESCRIPTOR_SIZE + size_of::<TransferPageHeader>() + size_of_val(ranges),
                PACKET_FLAG_COMPLETION_REQUESTED,
            ),
        };
        let msg_len = (packet.size + header_size).div_ceil(8) * 8;
        let total_msg_len = (msg_len + size_of::<Footer>()) as u32;
        if total_msg_len >= self.inner.len() - 8 {
            return Err(WriteError::Corrupt(Error::InvalidMessageLength));
        }
        let inp = ptrs.next_in;
        let mut outp = ptrs.cached_out;
        if self.inner.free(inp, outp) < total_msg_len {
            outp = self
                .inner
                .validate(self.inner.control().outp().load(Ordering::Relaxed))?;
            if self.inner.free(inp, outp) < total_msg_len {
                return Err(WriteError::Full(total_msg_len as usize));
            }
            ptrs.cached_out = outp;
        }
        let desc = PacketDescriptor {
            packet_type,
            data_offset8: header_size as u16 / 8,
            length8: (msg_len / 8) as u16,
            flags,
            transaction_id: packet.transaction_id,
        };

        let footer = Footer {
            reserved: 0,
            offset: inp,
        };

        let off = inp as usize;
        self.inner.mem.write_aligned(off, desc.as_bytes());
        match packet.typ {
            OutgoingPacketType::GpaDirect(ranges) => {
                let mut writer = RingRange {
                    off: (off + DESCRIPTOR_SIZE) as u32,
                    size: header_size as u32,
                }
                .writer(self);
                let gpa_header = GpaDirectHeader {
                    reserved: 0,
                    range_count: ranges.len() as u32,
                };
                writer
                    .write(gpa_header.as_bytes())
                    .map_err(|_| WriteError::Corrupt(Error::InvalidMessageLength))?;

                for range in ranges {
                    let gpa_rng = GpaRange {
                        len: range.len() as u32,
                        offset: range.offset() as u32,
                    };
                    writer
                        .write(gpa_rng.as_bytes())
                        .map_err(|_| WriteError::Corrupt(Error::InvalidMessageLength))?;
                    writer
                        .write(range.gpns().as_bytes())
                        .map_err(|_| WriteError::Corrupt(Error::InvalidMessageLength))?;
                }
            }
            OutgoingPacketType::TransferPages(tp_id, ranges) => {
                let tp_header = TransferPageHeader {
                    transfer_page_set_id: tp_id,
                    reserved: 0,
                    range_count: ranges.len() as u32,
                };
                self.inner
                    .mem
                    .write_aligned(off + DESCRIPTOR_SIZE, tp_header.as_bytes());
                for (i, range) in ranges.iter().enumerate() {
                    self.inner.mem.write_aligned(
                        off + DESCRIPTOR_SIZE + size_of_val(&tp_header) + i * 8,
                        range.as_bytes(),
                    );
                }
            }
            _ => (),
        }

        self.inner
            .mem
            .write_aligned(off + msg_len, footer.as_bytes());
        ptrs.next_in = self.inner.add_pointer(inp, total_msg_len);
        Ok(RingRange {
            off: inp + header_size as u32,
            size: packet.size as u32,
        })
    }
}

struct InnerRing<M: RingMem> {
    mem: M,
    size: u32,
}

impl<M: RingMem> Inspect for InnerRing<M> {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond()
            .hex("ring_size", self.size)
            .field("control", self.control());
    }
}

/// Inspects ring buffer state without creating an IncomingRing or OutgoingRing
/// structure.
///
/// # Panics
///
/// Panics if control_page is not aligned.
pub fn inspect_ring(control_page: &guestmem::Page, response: &mut inspect::Response<'_>) {
    let control = Control::from_page(control_page).expect("control page is not aligned");
    response.field("control", control);
}

/// Returns whether a ring buffer is in a state where the receiving end might
/// need a signal.
pub fn reader_needs_signal(control_page: &guestmem::Page) -> bool {
    Control::from_page(control_page).is_some_and(|control| {
        control.interrupt_mask().load(Ordering::Relaxed) == 0
            && (control.inp().load(Ordering::Relaxed) != control.outp().load(Ordering::Relaxed))
    })
}

/// Returns whether a ring buffer is in a state where the sending end might need
/// a signal.
pub fn writer_needs_signal(control_page: &guestmem::Page, ring_size: u32) -> bool {
    Control::from_page(control_page).is_some_and(|control| {
        let pending_size = control.pending_send_size().load(Ordering::Relaxed);
        pending_size != 0
            && ring_free(
                ring_size,
                control.inp().load(Ordering::Relaxed),
                control.outp().load(Ordering::Relaxed),
            ) >= pending_size
    })
}

impl<M: RingMem> Debug for InnerRing<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InnerRing")
            .field("control", &self.control())
            .field("size", &self.size)
            .finish()
    }
}

impl<M: RingMem> InnerRing<M> {
    pub fn new(mem: M) -> Result<Self, Error> {
        let ring_size = u32::try_from(mem.len()).map_err(|_| Error::InvalidRingMemory)?;
        if ring_size % 4096 != 0 {
            return Err(Error::InvalidRingMemory);
        }
        let ring = InnerRing {
            mem,
            size: ring_size,
        };
        Ok(ring)
    }

    fn control(&self) -> Control<'_> {
        Control(self.mem.control())
    }

    fn len(&self) -> u32 {
        self.size
    }

    fn validate(&self, p: u32) -> Result<u32, Error> {
        if p >= self.size || !p.is_multiple_of(8) {
            Err(Error::InvalidRingPointer)
        } else {
            Ok(p)
        }
    }

    fn add_pointer(&self, p: u32, off: u32) -> u32 {
        let np = p + off;
        if np >= self.size {
            assert!(np < self.size * 2);
            np - self.size
        } else {
            np
        }
    }

    fn available(&self, inp: u32, outp: u32) -> u32 {
        if inp > outp {
            // |____outp....inp_____|
            inp - outp
        } else {
            // |....inp____outp.....|
            self.size + inp - outp
        }
    }

    fn free(&self, inp: u32, outp: u32) -> u32 {
        ring_free(self.size, inp, outp)
    }
}

fn ring_free(size: u32, inp: u32, outp: u32) -> u32 {
    // It's not possible to fully fill the ring since that state would be
    // indistinguishable from the empty ring. So subtract 8 bytes from the
    // result.
    if outp > inp {
        // |....inp____outp.....|
        outp - inp - 8
    } else {
        // |____outp....inp_____|
        size - (inp - outp) - 8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_simple<T: RingMem>(out_ring: &mut OutgoingRing<T>, buf: &[u8]) -> Option<bool> {
        let mut outgoing = out_ring.outgoing().unwrap();
        match out_ring.write(
            &mut outgoing,
            &OutgoingPacket {
                typ: OutgoingPacketType::InBandNoCompletion,
                size: buf.len(),
                transaction_id: 0,
            },
        ) {
            Ok(range) => {
                range.writer(out_ring).write(buf).unwrap();
                Some(out_ring.commit_write(&mut outgoing))
            }
            Err(WriteError::Full(_)) => None,
            Err(err) => panic!("{}", err),
        }
    }

    fn read_simple<T: RingMem>(in_ring: &mut IncomingRing<T>) -> (Vec<u8>, bool) {
        let mut incoming = in_ring.incoming().unwrap();
        let msg = in_ring
            .read(&mut incoming)
            .unwrap()
            .payload
            .reader(in_ring)
            .read_all()
            .unwrap();
        let signal = in_ring.commit_read(&mut incoming);
        (msg, signal)
    }

    #[test]
    fn test_ring() {
        let rmem = FlatRingMem::new(16384);
        let mut in_ring = IncomingRing::new(&rmem).unwrap();
        in_ring.set_interrupt_mask(false);
        let mut out_ring = OutgoingRing::new(&rmem).unwrap();

        let p = &[1, 2, 3, 4, 5, 6, 7, 8];
        assert!(write_simple(&mut out_ring, p).unwrap());

        let (msg, signal) = read_simple(&mut in_ring);
        assert!(!signal);

        assert_eq!(p, &msg[..]);
    }

    #[test]
    fn test_interrupt_mask() {
        let rmem = FlatRingMem::new(16384);
        let mut in_ring = IncomingRing::new(&rmem).unwrap();
        let mut out_ring = OutgoingRing::new(&rmem).unwrap();

        // Interrupts are masked, so no signal is expected.
        assert!(!write_simple(&mut out_ring, &[1, 2, 3]).unwrap());
        assert!(!read_simple(&mut in_ring).1);

        // Unmask interrupts, then try again, expecting a signal this time.
        in_ring.set_interrupt_mask(false);
        assert!(write_simple(&mut out_ring, &[1, 2, 3]).unwrap());
        assert!(!read_simple(&mut in_ring).1);
    }

    #[test]
    fn test_pending_send_size() {
        let rmem = FlatRingMem::new(16384);
        let mut in_ring = IncomingRing::new(&rmem).unwrap();
        let mut out_ring = OutgoingRing::new(&rmem).unwrap();

        // Fill the ring up with some packets.
        write_simple(&mut out_ring, &[1; 4000]).unwrap();
        write_simple(&mut out_ring, &[2; 4000]).unwrap();
        write_simple(&mut out_ring, &[3; 4000]).unwrap();
        write_simple(&mut out_ring, &[4; 4000]).unwrap();
        assert!(write_simple(&mut out_ring, &[5; 4000]).is_none());

        // No pending send size yet.
        assert!(!read_simple(&mut in_ring).1);

        // Fill the ring back up.
        write_simple(&mut out_ring, &[5; 4000]).unwrap();
        assert!(write_simple(&mut out_ring, &[6; 4000]).is_none());

        // Set a pending send size for two packets worth of space (packet size +
        // 16 bytes for the descriptor and 8 bytes for the footer).
        out_ring.set_pending_send_size(4024 * 2).unwrap();

        // There should be a signal after two packets, then no more signals.
        assert!(!read_simple(&mut in_ring).1);
        assert!(read_simple(&mut in_ring).1);
        assert!(!read_simple(&mut in_ring).1);
    }
}
