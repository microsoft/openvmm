// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of [`RxBufferAccess`] and friends on top of the receive
//! buffers.

use crate::rndisprot;
use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use guestmem::LockedPages;
use net_backend::BufferAccess;
use net_backend::L4Protocol;
use net_backend::RxBufferSegment;
use net_backend::RxChecksumState;
use net_backend::RxId;
use net_backend::RxMetadata;
use safeatomic::AtomicSliceOps;
use std::ops::Range;
use std::sync::Arc;
use thiserror::Error;
use vmbus_channel::gpadl::GpadlView;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

const PAGE_SIZE: usize = 4096;
const PAGE_SIZE32: u32 = 4096;

/// Error returned when a write to the receive buffer would exceed the locked
/// page range.
#[derive(Debug, Error)]
#[error(
    "rx buffer write overflow: offset={offset} len={len} \
     total_size={total_size}"
)]
struct RxBufferWriteOverflow {
    offset: usize,
    len: usize,
    total_size: usize,
}

/// Validates that `offset + len` does not overflow `usize` and
/// `[offset, offset + len)` fits within `total_size`.
fn check_rx_write_bounds(
    offset: usize,
    len: usize,
    total_size: usize,
) -> Result<(), RxBufferWriteOverflow> {
    let rx_overflow_err = || RxBufferWriteOverflow {
        offset,
        len,
        total_size,
    };
    let end = offset.checked_add(len).ok_or_else(rx_overflow_err)?;
    if end > total_size {
        return Err(rx_overflow_err());
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum GuestBuffersError {
    #[error("invalid mtu {mtu}")]
    InvalidMtu { mtu: u32 },
    #[error("sub_allocation_size {sub_allocation_size} is too small for mtu {mtu}")]
    SubAllocationTooSmall { sub_allocation_size: u32, mtu: u32 },
    #[error("GPADL has no ranges")]
    EmptyGpadl,
    #[error("failed to lock guest page numbers")]
    GpnLock(#[source] GuestMemoryError),
}

/// A type providing access to the netvsp receive buffer.
pub struct GuestBuffers {
    mem: GuestMemory,
    _gpadl: GpadlView,
    locked_pages: LockedPages,
    gpns: Vec<u64>,
    sub_allocation_size: u32,
    mtu: u32,
}

/// A per-queue wrapper around guest buffers. The receive buffer is shared
/// across all queues, but they are statically partitioned into per-queue
/// suballocations.
pub struct BufferPool {
    buffers: Arc<GuestBuffers>,
    rx_vlan_count: u64,
}

impl BufferPool {
    pub fn new(buffers: Arc<GuestBuffers>) -> Self {
        Self {
            buffers,
            rx_vlan_count: 0,
        }
    }

    fn offset(&self, id: RxId) -> u32 {
        id.0 * self.buffers.sub_allocation_size
    }

    /// Returns and resets the number of RX packets with VLAN metadata
    /// observed since the last call.
    pub fn take_rx_vlan_count(&mut self) -> u64 {
        std::mem::take(&mut self.rx_vlan_count)
    }
}

impl GuestBuffers {
    /// Validates that the GPADL and sub_allocation_size are compatible with the MTU
    /// without performing any allocations.
    pub fn validate_config(
        gpadl: &GpadlView,
        sub_allocation_size: u32,
        mtu: u32,
    ) -> Result<(), GuestBuffersError> {
        if gpadl.first().is_none() {
            return Err(GuestBuffersError::EmptyGpadl);
        }
        mtu.checked_add(RX_HEADER_LEN)
            .and_then(|v| v.checked_add(BROKEN_CO_NETVSC_FOOTER_LEN))
            .ok_or(GuestBuffersError::InvalidMtu { mtu })?;
        if sub_allocation_size < sub_allocation_size_for_mtu(mtu) {
            return Err(GuestBuffersError::SubAllocationTooSmall {
                sub_allocation_size,
                mtu,
            });
        }
        Ok(())
    }

    pub fn new(
        mem: GuestMemory,
        gpadl: GpadlView,
        sub_allocation_size: u32,
        mtu: u32,
    ) -> Result<Self, GuestBuffersError> {
        Self::validate_config(&gpadl, sub_allocation_size, mtu)?;

        let gpns = gpadl.first().unwrap().gpns().to_vec();
        let locked_pages = mem
            .lock_gpns(guestmem::AccessType::Write, false, &gpns)
            .map_err(GuestBuffersError::GpnLock)?;
        Ok(Self {
            mem,
            _gpadl: gpadl,
            gpns,
            sub_allocation_size,
            locked_pages,
            mtu,
        })
    }

    fn write_at(&self, offset: u32, buf: &[u8]) -> Result<(), RxBufferWriteOverflow> {
        self.write_segments_at(offset, &[buf])
    }

    fn write_segments_at(
        &self,
        offset: u32,
        segments: &[&[u8]],
    ) -> Result<(), RxBufferWriteOverflow> {
        let mut offset = offset as usize;
        let total_size = self
            .locked_pages
            .pages()
            .len()
            .checked_mul(PAGE_SIZE)
            .ok_or(RxBufferWriteOverflow {
                offset,
                len: usize::MAX,
                total_size: usize::MAX,
            })?;
        let len = segments
            .iter()
            .try_fold(0usize, |len, segment| len.checked_add(segment.len()))
            .ok_or(RxBufferWriteOverflow {
                offset,
                len: usize::MAX,
                total_size,
            })?;
        check_rx_write_bounds(offset, len, total_size)?;
        for &segment in segments {
            let mut buf = segment;
            while !buf.is_empty() {
                let len = (PAGE_SIZE - offset % PAGE_SIZE).min(buf.len());
                let (this, next) = buf.split_at(len);
                self.locked_pages.pages()[offset / PAGE_SIZE][offset % PAGE_SIZE..][..len]
                    .atomic_write(this);
                buf = next;
                offset += len;
            }
        }
        Ok(())
    }
}

// Reserve this many bytes for the RNDIS headers.
const RX_HEADER_LEN: u32 = 256;

// The last 36 bytes of each suballocation cannot be used due to a bug in netvsc
// in newer versions of Windows.
const BROKEN_CO_NETVSC_FOOTER_LEN: u32 = 36;

/// Computes the suballocation size needed for the specified MTU.
pub const fn sub_allocation_size_for_mtu(mtu: u32) -> u32 {
    RX_HEADER_LEN + mtu + BROKEN_CO_NETVSC_FOOTER_LEN
}

/// Computes the buffer segments for accessing a range of the receive buffer.
fn compute_buffer_segments(v: &mut Vec<RxBufferSegment>, gpns: &[u64], mut range: Range<u32>) {
    while !range.is_empty() {
        let start_page = range.start / PAGE_SIZE32;
        let start_offset = range.start % PAGE_SIZE32;
        let max_page = (range.end - 1) / PAGE_SIZE32 + 1;
        let mut end_page = start_page + 1;
        while end_page < max_page && gpns[end_page as usize] == gpns[end_page as usize - 1] + 1 {
            end_page += 1;
        }

        let gpa = gpns[start_page as usize] * PAGE_SIZE as u64 + start_offset as u64;
        let end = (end_page * PAGE_SIZE32).min(range.end);

        v.push(RxBufferSegment {
            gpa,
            len: (end - range.start),
        });

        range.start = end;
    }
}

impl BufferAccess for BufferPool {
    fn guest_memory(&self) -> &GuestMemory {
        &self.buffers.mem
    }

    fn push_guest_addresses(&self, id: RxId, buf: &mut Vec<RxBufferSegment>) {
        let offset = self.offset(id);
        compute_buffer_segments(
            buf,
            &self.buffers.gpns,
            offset + RX_HEADER_LEN..offset + RX_HEADER_LEN + self.buffers.mtu,
        );
    }

    fn capacity(&self, _id: RxId) -> u32 {
        self.buffers.mtu
    }

    fn write_data(&mut self, id: RxId, data: &[u8]) {
        if data.len() > self.capacity(id) as usize {
            tracelimit::warn_ratelimited!(
                rx_id = id.0,
                data_len = data.len(),
                mtu = self.capacity(id),
                "rx data exceeds capacity, dropping",
            );
            return;
        }
        if let Err(err) = self.buffers.write_at(self.offset(id) + RX_HEADER_LEN, data) {
            tracelimit::warn_ratelimited!(
                error = &err as &dyn std::error::Error,
                rx_id = id.0,
                data_len = data.len(),
                "rx buffer write overflow in write_data",
            );
        }
    }

    fn write_packet(&mut self, id: RxId, metadata: &RxMetadata, data: &[u8]) {
        self.write_packet_segments(id, metadata, &[data]);
    }

    fn write_packet_segments(&mut self, id: RxId, metadata: &RxMetadata, segments: &[&[u8]]) {
        if segments
            .iter()
            .try_fold(self.capacity(id) as usize, |remaining, segment| {
                remaining.checked_sub(segment.len())
            })
            .is_none()
        {
            tracelimit::warn_ratelimited!(
                rx_id = id.0,
                segment_count = segments.len(),
                mtu = self.capacity(id),
                "rx packet segments exceed capacity, dropping",
            );
            return;
        }
        if let Err(err) = self
            .buffers
            .write_segments_at(self.offset(id) + RX_HEADER_LEN, segments)
        {
            tracelimit::warn_ratelimited!(
                error = &err as &dyn std::error::Error,
                rx_id = id.0,
                "rx buffer write overflow in write_packet_segments",
            );
            return;
        }
        self.write_header(id, metadata);
    }

    fn write_header(&mut self, id: RxId, metadata: &RxMetadata) {
        #[repr(C)]
        #[derive(zerocopy::IntoBytes, Immutable, KnownLayout, Debug)]
        struct Header {
            header: rndisprot::MessageHeader,
            packet: rndisprot::Packet,
        }

        #[repr(C)]
        #[derive(zerocopy::IntoBytes, Immutable, KnownLayout, Debug)]
        struct PerPacketInfo {
            header: rndisprot::PerPacketInfo,
            payload: u32,
        }

        let mut ppi_count = 1;
        let checksum = rndisprot::RxTcpIpChecksumInfo::new_zeroed()
            .set_ip_checksum_failed(metadata.ip_checksum == RxChecksumState::Bad)
            .set_ip_checksum_succeeded(metadata.ip_checksum.is_valid())
            .set_ip_checksum_value_invalid(
                metadata.ip_checksum == RxChecksumState::ValidatedButWrong,
            )
            .set_tcp_checksum_failed(
                metadata.l4_protocol == L4Protocol::Tcp
                    && metadata.l4_checksum == RxChecksumState::Bad,
            )
            .set_tcp_checksum_succeeded(
                metadata.l4_protocol == L4Protocol::Tcp && metadata.l4_checksum.is_valid(),
            )
            .set_tcp_checksum_value_invalid(
                metadata.l4_protocol == L4Protocol::Tcp
                    && metadata.l4_checksum == RxChecksumState::ValidatedButWrong,
            )
            .set_udp_checksum_failed(
                metadata.l4_protocol == L4Protocol::Udp
                    && metadata.l4_checksum == RxChecksumState::Bad,
            )
            .set_udp_checksum_succeeded(
                metadata.l4_protocol == L4Protocol::Udp && metadata.l4_checksum.is_valid(),
            );
        let checksum_ppi = PerPacketInfo {
            header: rndisprot::PerPacketInfo {
                size: size_of::<PerPacketInfo>() as u32,
                typ: rndisprot::PPI_TCP_IP_CHECKSUM,
                per_packet_information_offset: size_of::<rndisprot::PerPacketInfo>() as u32,
            },
            payload: checksum.0,
        };

        let vlan = if let Some(vlan_info) = metadata.vlan {
            self.rx_vlan_count += 1;
            ppi_count += 1;

            Some(PerPacketInfo {
                header: rndisprot::PerPacketInfo {
                    size: size_of::<PerPacketInfo>() as u32,
                    typ: rndisprot::PPI_VLAN,
                    per_packet_information_offset: size_of::<rndisprot::PerPacketInfo>() as u32,
                },
                payload: Into::<rndisprot::EthVlanInfo>::into(vlan_info).into(),
            })
        } else {
            None
        };

        let header = Header {
            header: rndisprot::MessageHeader {
                message_type: rndisprot::MESSAGE_TYPE_PACKET_MSG,
                // Always claim the full suballocation length to avoid needing
                // to track this more accurately. This needs to match the
                // transfer page length but is not otherwise constrained for
                // packet messages.
                message_length: self.buffers.sub_allocation_size,
            },
            packet: rndisprot::Packet {
                data_offset: RX_HEADER_LEN - size_of::<rndisprot::MessageHeader>() as u32
                    + metadata.offset as u32,
                data_length: metadata.len as u32,
                oob_data_offset: 0,
                oob_data_length: 0,
                num_oob_data_elements: 0,
                per_packet_info_offset: size_of::<rndisprot::Packet>() as u32,
                per_packet_info_length: ppi_count * size_of::<PerPacketInfo>() as u32,
                vc_handle: 0,
                reserved: 0,
            },
        };

        let segments = [
            header.as_bytes(),
            checksum_ppi.as_bytes(),
            vlan.as_ref().map_or(&[][..], |ppi| ppi.as_bytes()),
        ];
        if let Err(err) = self.buffers.write_segments_at(self.offset(id), &segments) {
            tracelimit::warn_ratelimited!(
                error = &err as &dyn std::error::Error,
                rx_id = id.0,
                "rx buffer write overflow in write_header",
            );
        }
        static_assertions::const_assert!(
            (size_of::<Header>() + 2 * size_of::<PerPacketInfo>()) < RX_HEADER_LEN as usize
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::buffers::BufferPool;
    use crate::buffers::GuestBuffers;
    use crate::buffers::GuestBuffersError;
    use crate::buffers::PAGE_SIZE;
    use crate::buffers::RX_HEADER_LEN;
    use crate::buffers::check_rx_write_bounds;
    use crate::buffers::compute_buffer_segments;
    use crate::buffers::sub_allocation_size_for_mtu;
    use guestmem::GuestMemory;
    use net_backend::BufferAccess;
    use net_backend::RxBufferSegment;
    use net_backend::RxId;
    use net_backend::RxMetadata;
    use net_backend::VlanMetadata;
    use std::sync::Arc;
    use test_with_tracing::test;
    use vmbus_channel::gpadl::GpadlMap;
    use vmbus_core::protocol::GpadlId;
    use vmbus_ring::gparange::GpaRange;
    use vmbus_ring::gparange::MultiPagedRangeBuf;
    use zerocopy::IntoBytes;

    fn buffer_pool(mtu: u32) -> BufferPool {
        let memory = GuestMemory::allocate(PAGE_SIZE);
        memory.fill_at(0, 0xa5, PAGE_SIZE).unwrap();
        let range = GpaRange {
            len: PAGE_SIZE as u32,
            offset: 0,
        };
        let range = MultiPagedRangeBuf::from_range_buffer(
            1,
            vec![u64::from_le_bytes(range.as_bytes().try_into().unwrap()), 0],
        )
        .unwrap();
        let map = GpadlMap::new();
        let id = GpadlId(4);
        map.add(id, range);
        let buffers = GuestBuffers::new(
            memory,
            map.view().map(id).unwrap(),
            sub_allocation_size_for_mtu(mtu),
            mtu,
        )
        .unwrap();
        BufferPool::new(Arc::new(buffers))
    }

    fn read_buffer(pool: &BufferPool) -> Vec<u8> {
        let mut bytes = vec![0; PAGE_SIZE];
        pool.guest_memory().read_at(0, &mut bytes).unwrap();
        bytes
    }

    #[test]
    fn oversized_rx_data_leaves_memory_unchanged() {
        let mut pool = buffer_pool(64);
        pool.write_data(RxId(0), &[0x11; 65]);
        assert_eq!(read_buffer(&pool), vec![0xa5; PAGE_SIZE]);
    }

    #[test]
    fn exact_capacity_rx_data_preserves_adjacent_memory() {
        let mut pool = buffer_pool(64);
        pool.write_data(RxId(0), &[0x11; 64]);
        let mut expected = vec![0xa5; PAGE_SIZE];
        expected[RX_HEADER_LEN as usize..RX_HEADER_LEN as usize + 64].fill(0x11);
        assert_eq!(read_buffer(&pool), expected);
    }

    #[test]
    fn oversized_rx_packets_leave_header_and_data_unchanged() {
        let metadata = RxMetadata {
            len: 65,
            vlan: Some(VlanMetadata::new().with_vlan_id(7)),
            ..Default::default()
        };
        let mut pool = buffer_pool(64);
        pool.write_packet(RxId(0), &metadata, &[0x11; 65]);
        assert_eq!(read_buffer(&pool), vec![0xa5; PAGE_SIZE]);
        pool.write_packet_segments(RxId(0), &metadata, &[&[0x11; 32], &[0x22; 33]]);
        assert_eq!(read_buffer(&pool), vec![0xa5; PAGE_SIZE]);
        assert_eq!(pool.take_rx_vlan_count(), 0);
    }

    #[test]
    fn exact_capacity_rx_segments_match_contiguous_packet() {
        let metadata = RxMetadata {
            len: 64,
            vlan: Some(VlanMetadata::new().with_vlan_id(7)),
            ..Default::default()
        };
        let mut contiguous = buffer_pool(64);
        let mut segmented = buffer_pool(64);
        let mut data = [0x11; 64];
        data[32..].fill(0x22);
        contiguous.write_packet(RxId(0), &metadata, &data);
        segmented.write_packet_segments(RxId(0), &metadata, &[&data[..32], &[], &data[32..]]);
        let bytes = read_buffer(&segmented);
        assert_eq!(bytes, read_buffer(&contiguous));
        assert_eq!(
            &bytes[RX_HEADER_LEN as usize..RX_HEADER_LEN as usize + 64],
            &data
        );
        assert!(
            bytes[RX_HEADER_LEN as usize + 64..]
                .iter()
                .all(|&b| b == 0xa5)
        );
        assert_eq!(segmented.take_rx_vlan_count(), 1);
        assert_eq!(contiguous.take_rx_vlan_count(), 1);
    }

    #[test]
    fn segmented_backing_buffer_overflow_is_rejected_before_writing() {
        let pool = buffer_pool(64);
        pool.buffers
            .write_segments_at(PAGE_SIZE as u32 - 2, &[&[0x11; 2], &[0x22]])
            .unwrap_err();
        assert_eq!(read_buffer(&pool), vec![0xa5; PAGE_SIZE]);
        pool.buffers
            .write_at(PAGE_SIZE as u32 - 1, &[0x11])
            .unwrap();
        assert_eq!(read_buffer(&pool)[PAGE_SIZE - 1], 0x11);
    }

    #[test]
    fn rx_packet_backing_buffer_overflow_leaves_header_and_data_unchanged() {
        let mut pool = buffer_pool(PAGE_SIZE as u32);
        let data = [0x11; PAGE_SIZE];
        let metadata = RxMetadata {
            len: data.len(),
            vlan: Some(VlanMetadata::new().with_vlan_id(7)),
            ..Default::default()
        };
        pool.write_data(RxId(0), &data);
        assert_eq!(read_buffer(&pool), vec![0xa5; PAGE_SIZE]);
        pool.write_packet_segments(
            RxId(0),
            &metadata,
            &[&data[..PAGE_SIZE / 2], &data[PAGE_SIZE / 2..]],
        );
        assert_eq!(read_buffer(&pool), vec![0xa5; PAGE_SIZE]);
        assert_eq!(pool.take_rx_vlan_count(), 0);
    }

    /// Verify that inconsistent sub_allocation_size and MTU from saved state
    /// returns an error instead of panicking.
    #[test]
    fn sub_allocation_too_small_for_mtu() {
        let default_mtu = 1514;
        let max_mtu = 9216;
        let sub_alloc_for_default = sub_allocation_size_for_mtu(default_mtu);

        // The sub_allocation for default MTU must be smaller than for max MTU.
        assert!(sub_alloc_for_default < sub_allocation_size_for_mtu(max_mtu));

        // Build a multipaged ranged buffer.
        let num_pages = 16;
        let hdr = GpaRange {
            len: (num_pages * 4096) as u32,
            offset: 0,
        };
        let mut buf = vec![u64::from_le_bytes(hdr.as_bytes().try_into().unwrap())];
        // Append one GPN per page.
        buf.extend((0..num_pages).map(|i| i as u64));
        let multipaged_ranged_buf = MultiPagedRangeBuf::from_range_buffer(1, buf).unwrap();

        // Build a minimal GpadlView (won't be accessed — the check fires first).
        let gpadl_map = GpadlMap::new();
        let gpadl_id = GpadlId(1);
        gpadl_map.add(gpadl_id, multipaged_ranged_buf);
        let gpadl_view = gpadl_map.view().map(gpadl_id).unwrap();

        let mem = GuestMemory::empty();
        let result = GuestBuffers::new(mem, gpadl_view, sub_alloc_for_default, max_mtu);
        match result {
            Err(GuestBuffersError::SubAllocationTooSmall { .. }) => {}
            Err(e) => panic!("expected SubAllocationTooSmall, got {e}"),
            Ok(_) => panic!("expected SubAllocationTooSmall, got Ok"),
        }
    }

    /// Verify that an MTU near u32::MAX returns InvalidMtu instead of
    /// wrapping the sub_allocation_size calculation.
    #[test]
    fn overflowing_mtu_returns_error() {
        let num_pages = 16;
        let hdr = GpaRange {
            len: (num_pages * 4096) as u32,
            offset: 0,
        };
        let mut buf = vec![u64::from_le_bytes(hdr.as_bytes().try_into().unwrap())];
        buf.extend((0..num_pages).map(|i| i as u64));
        let multipaged_ranged_buf = MultiPagedRangeBuf::from_range_buffer(1, buf).unwrap();

        let gpadl_map = GpadlMap::new();
        let gpadl_id = GpadlId(3);
        gpadl_map.add(gpadl_id, multipaged_ranged_buf);
        let gpadl_view = gpadl_map.view().map(gpadl_id).unwrap();

        // An MTU of u32::MAX would overflow the sub_allocation_size addition.
        let result = GuestBuffers::validate_config(&gpadl_view, 1806, u32::MAX);
        match result {
            Err(GuestBuffersError::InvalidMtu { .. }) => {}
            Err(e) => panic!("expected InvalidMtu, got {e}"),
            Ok(_) => panic!("expected InvalidMtu, got Ok"),
        }
    }

    /// Verify that a GPADL with zero ranges returns EmptyGpadl instead of
    /// panicking.
    #[test]
    fn empty_gpadl_returns_error() {
        let multipaged_ranged_buf = MultiPagedRangeBuf::from_range_buffer(0, vec![]).unwrap();

        let gpadl_map = GpadlMap::new();
        let gpadl_id = GpadlId(2);
        gpadl_map.add(gpadl_id, multipaged_ranged_buf);
        let gpadl_view = gpadl_map.view().map(gpadl_id).unwrap();

        let mem = GuestMemory::empty();
        let result = GuestBuffers::new(mem, gpadl_view, 1806, 1514);
        match result {
            Err(GuestBuffersError::EmptyGpadl) => {}
            Err(e) => panic!("expected EmptyGpadl, got {e}"),
            Ok(_) => panic!("expected EmptyGpadl, got Ok"),
        }
    }

    #[test]
    fn test_buffer_segments() {
        fn check(addrs: &[RxBufferSegment], check: &[(u64, u32)]) {
            assert_eq!(addrs.len(), check.len());
            let v: Vec<_> = addrs.iter().map(|range| (range.gpa, range.len)).collect();
            assert_eq!(v.as_slice(), check);
        }

        let gpns = [1, 3, 4, 5, 8];
        let cases = [
            (0x1..0x5, &[(0x1001, 4)][..]),
            (0x1..0x1005, &[(0x1001, 0xfff), (0x3000, 5)]),
            (0x1001..0x2005, &[(0x3001, 0x1004)]),
            (0x1001..0x5000, &[(0x3001, 0x2fff), (0x8000, 0x1000)]),
        ];
        for (range, data) in cases {
            let mut v = Vec::new();
            compute_buffer_segments(&mut v, &gpns, range);
            check(&v, data);
        }
    }

    #[test]
    fn test_check_rx_write_bounds() {
        // Empty write at the end is fine.
        check_rx_write_bounds(0x1000, 0, 0x1000).unwrap();
        // Write that exactly fills the buffer is fine.
        check_rx_write_bounds(0, 0x1000, 0x1000).unwrap();
        // Write entirely inside the buffer is fine.
        check_rx_write_bounds(0x100, 0x200, 0x1000).unwrap();
        // Write that runs one byte past the end is rejected.
        check_rx_write_bounds(0x1000, 1, 0x1000).unwrap_err();
        check_rx_write_bounds(0xfff, 2, 0x1000).unwrap_err();
        // Offset already past the end is rejected.
        check_rx_write_bounds(0x2000, 0, 0x1000).unwrap_err();
        // Length that overflows usize when added to offset is rejected.
        check_rx_write_bounds(usize::MAX, 1, usize::MAX).unwrap_err();
        check_rx_write_bounds(1, usize::MAX, usize::MAX).unwrap_err();
    }
}
