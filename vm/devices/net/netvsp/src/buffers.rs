// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of [`RxBufferAccess`] and friends on top of the receive
//! buffers.

use crate::rndisprot;
use crate::MAX_MTU;
use arrayvec::ArrayVec;
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
use vmbus_channel::gpadl::GpadlView;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

const PAGE_SIZE: usize = 4096;
const PAGE_SIZE32: u32 = 4096;

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
    buffer_segments: ArrayVec<RxBufferSegment, MAX_RX_SEGMENTS>,
}

impl BufferPool {
    pub fn new(buffers: Arc<GuestBuffers>) -> Self {
        Self {
            buffers,
            buffer_segments: ArrayVec::new(),
        }
    }

    fn offset(&self, id: RxId) -> u32 {
        id.0 * self.buffers.sub_allocation_size
    }
}

impl GuestBuffers {
    pub fn new(
        mem: GuestMemory,
        gpadl: GpadlView,
        sub_allocation_size: u32,
        mtu: u32,
    ) -> Result<Self, GuestMemoryError> {
        assert!(sub_allocation_size >= sub_allocation_size_for_mtu(mtu));

        let gpns = gpadl.first().unwrap().gpns().to_vec();
        let locked_pages = mem.lock_gpns(false, &gpns)?;
        Ok(Self {
            mem,
            _gpadl: gpadl,
            gpns,
            sub_allocation_size,
            locked_pages,
            mtu,
        })
    }

    fn write_at(&self, offset: u32, mut buf: &[u8]) {
        let mut offset = offset as usize;
        while !buf.is_empty() {
            let len = (PAGE_SIZE - offset % PAGE_SIZE).min(buf.len());
            let (this, next) = buf.split_at(len);
            self.locked_pages.pages()[offset / PAGE_SIZE][offset % PAGE_SIZE..][..len]
                .atomic_write(this);
            buf = next;
            offset += len;
        }
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

const MAX_RX_SEGMENTS: usize =
    ((sub_allocation_size_for_mtu(MAX_MTU) + (PAGE_SIZE32 - 1) * 2) / PAGE_SIZE32) as usize;

/// Comutes the buffer segments for accessing
fn compute_buffer_segments(
    v: &mut ArrayVec<RxBufferSegment, MAX_RX_SEGMENTS>,
    gpns: &[u64],
    mut range: Range<u32>,
) {
    v.clear();
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

    fn guest_addresses(&mut self, id: RxId) -> &[RxBufferSegment] {
        let offset = self.offset(id);
        compute_buffer_segments(
            &mut self.buffer_segments,
            &self.buffers.gpns,
            offset + RX_HEADER_LEN..offset + RX_HEADER_LEN + self.buffers.mtu,
        );
        &self.buffer_segments
    }

    fn capacity(&self, _id: RxId) -> u32 {
        self.buffers.mtu
    }

    fn write_data(&mut self, id: RxId, data: &[u8]) {
        self.buffers.write_at(self.offset(id) + RX_HEADER_LEN, data);
    }

    fn write_header(&mut self, id: RxId, metadata: &RxMetadata) {
        #[repr(C)]
        #[derive(zerocopy::IntoBytes, Immutable, KnownLayout, Debug)]
        struct Header {
            header: rndisprot::MessageHeader,
            packet: rndisprot::Packet,
            per_packet_info: PerPacketInfo,
        }

        #[repr(C)]
        #[derive(zerocopy::IntoBytes, Immutable, KnownLayout, Debug)]
        struct PerPacketInfo {
            header: rndisprot::PerPacketInfo,
            checksum: rndisprot::RxTcpIpChecksumInfo,
        }

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
                per_packet_info_length: size_of::<PerPacketInfo>() as u32,
                vc_handle: 0,
                reserved: 0,
            },
            per_packet_info: PerPacketInfo {
                header: rndisprot::PerPacketInfo {
                    size: size_of::<PerPacketInfo>() as u32,
                    typ: rndisprot::PPI_TCP_IP_CHECKSUM,
                    per_packet_information_offset: size_of::<rndisprot::PerPacketInfo>() as u32,
                },
                checksum,
            },
        };

        self.buffers.write_at(self.offset(id), header.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use crate::buffers::compute_buffer_segments;
    use arrayvec::ArrayVec;
    use net_backend::RxBufferSegment;

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
            let mut v = ArrayVec::new();
            compute_buffer_segments(&mut v, &gpns, range);
            check(&v, data);
        }
    }
}
