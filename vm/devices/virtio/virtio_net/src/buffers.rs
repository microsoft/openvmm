// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::NetworkFeaturesBank0;
use crate::VirtioNetHeader;
use crate::VirtioNetHeaderFlags;
use crate::header_size;
use guestmem::GuestMemory;
use inspect::Inspect;
use net_backend::BufferAccess;
use net_backend::ETHERNET_HEADER_LEN;
use net_backend::RxBufferSegment;
use net_backend::RxId;
use net_backend::RxMetadata;
use std::collections::VecDeque;
use std::num::NonZeroU16;
use virtio::VirtioQueueCallbackWork;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

struct RxPacket {
    work: VirtioQueueCallbackWork,
    used_len: Option<u32>,
    cap: u32,
    data_offset: u32,
    write_failed: bool,
}

/// Holds virtio buffers available for a network backend to send data to the client.
#[derive(Inspect)]
#[inspect(extra = "Self::inspect_extra")]
pub struct VirtioWorkPool {
    mem: GuestMemory,
    mergeable_rx_buffers: bool,
    guest_csum: bool,
    #[inspect(skip)]
    rx_packets: Vec<Option<RxPacket>>,
    #[inspect(skip)]
    rx_order: VecDeque<RxId>,
    #[inspect(skip)]
    rx_write_ranges: Vec<RxBufferSegment>,
    rx_capacity_drops: u64,
    rx_memory_write_drops: u64,
    rx_incomplete_drops: u64,
}

impl VirtioWorkPool {
    fn inspect_extra(&self, resp: &mut inspect::Response<'_>) {
        resp.field(
            "pending_rx_packets",
            self.rx_packets.iter().filter(|p| p.is_some()).count(),
        )
        .field("rx_capacity_drops", self.rx_capacity_drops)
        .field("rx_memory_write_drops", self.rx_memory_write_drops)
        .field("rx_incomplete_drops", self.rx_incomplete_drops);
    }

    /// Create a new instance.
    pub fn new(mem: GuestMemory, queue_size: u16, features: NetworkFeaturesBank0) -> Self {
        Self {
            mem,
            mergeable_rx_buffers: features.mrg_rxbuf(),
            guest_csum: features.guest_csum(),
            rx_packets: (0..queue_size).map(|_| None).collect(),
            rx_order: VecDeque::new(),
            rx_write_ranges: Vec::new(),
            rx_capacity_drops: 0,
            rx_memory_write_drops: 0,
            rx_incomplete_drops: 0,
        }
    }

    /// Returns a reference to the guest memory.
    pub fn mem(&self) -> &GuestMemory {
        &self.mem
    }

    /// Resets and reposts outstanding buffers in their original available-ring
    /// order after a backend restart. `buf` must be
    /// at least as big as the virtio queue size, passed to `new()`.
    ///
    /// Returns the number of entries written.
    pub fn fill_ready(&mut self, buf: &mut [RxId]) -> usize {
        assert!(buf.len() >= self.rx_packets.len());
        for (dest, &id) in buf.iter_mut().zip(&self.rx_order) {
            let packet = self.rx_packets[id.0 as usize].as_mut().unwrap();
            packet.used_len = None;
            packet.write_failed = false;
            packet.data_offset = header_size() as u32;
            *dest = id;
        }
        self.rx_order.len()
    }

    /// Add a virtio work instance to the buffers available for use.
    ///
    /// Returns `Err` with the work item if the chain has an invalid capacity.
    /// Mergeable buffers need room for the header; nonmergeable buffers also
    /// need room for an Ethernet header. The caller handles rejection in order.
    pub fn queue_work(
        &mut self,
        work: VirtioQueueCallbackWork,
    ) -> Result<RxId, VirtioQueueCallbackWork> {
        let idx = work.descriptor_index();
        let packet = &mut self.rx_packets[idx as usize];
        assert!(packet.is_none(), "RX descriptor {idx} is already tracked");
        let payload_length = work.get_payload_length(true);
        let Some(cap) = u32::try_from(payload_length)
            .ok()
            .and_then(|len| len.checked_sub(header_size() as u32))
            .filter(|&cap| self.mergeable_rx_buffers || cap >= ETHERNET_HEADER_LEN)
        else {
            tracelimit::warn_ratelimited!(
                len = payload_length,
                "dropping RX buffer: invalid capacity"
            );
            return Err(work);
        };
        *packet = Some(RxPacket {
            used_len: None,
            cap,
            data_offset: header_size() as u32,
            write_failed: false,
            work,
        });
        let id = RxId(idx.into());
        self.rx_order.push_back(id);
        Ok(id)
    }

    /// Take the RX work item for the given packet, returning it with the
    /// computed payload length. The caller is responsible for completing
    /// the descriptor via the queue.
    #[must_use = "caller must complete the returned work via VirtioQueue::complete"]
    pub fn take_rx_work(&mut self, rx_id: RxId) -> (VirtioQueueCallbackWork, u32, bool) {
        let index = self
            .rx_order
            .iter()
            .position(|id| id.0 == rx_id.0)
            .expect("valid packet index");
        self.rx_order.remove(index);
        let packet = self.rx_packets[rx_id.0 as usize]
            .take()
            .expect("valid packet index");
        let dropped = packet.write_failed || packet.used_len.is_none();
        let used_len = if dropped {
            if !packet.write_failed {
                self.rx_incomplete_drops += 1;
                tracelimit::warn_ratelimited!("dropping RX buffer: packet write not completed");
            }
            0
        } else {
            packet.used_len.unwrap()
        };
        (packet.work, used_len, dropped)
    }

    fn write_rx_header(&mut self, id: RxId, metadata: &RxMetadata, buffer_count: NonZeroU16) {
        let checksum = metadata.offload.checksum_metadata();
        let needs_checksum = checksum.is_some();
        let data_valid = self.guest_csum
            && !needs_checksum
            && metadata.ip_checksum.is_valid()
            && metadata.l4_checksum.is_valid();
        let flags = VirtioNetHeaderFlags::new()
            .with_needs_csum(needs_checksum)
            .with_data_valid(data_valid);
        let (gso_type, hdr_len, gso_size) = if let Some(gso) = metadata.offload.gso() {
            let protocol = match gso.protocol {
                net_backend::RxGsoProtocol::TcpV4 => crate::VirtioNetHeaderGsoProtocol::TCPV4,
                net_backend::RxGsoProtocol::TcpV6 => crate::VirtioNetHeaderGsoProtocol::TCPV6,
            };
            (
                crate::VirtioNetHeaderGso::new()
                    .with_protocol(protocol)
                    .into_bits(),
                gso.header_len,
                gso.max_segment_size,
            )
        } else {
            (0, 0, 0)
        };
        let (csum_start, csum_offset) = checksum
            .map(|checksum| (checksum.start, checksum.offset))
            .unwrap_or_default();
        let header = VirtioNetHeader {
            flags: flags.into(),
            gso_type,
            hdr_len,
            gso_size,
            csum_start,
            csum_offset,
            num_buffers: buffer_count.get(),
            ..FromZeros::new_zeroed()
        };
        let packet = self.rx_packets[id.0 as usize]
            .as_mut()
            .expect("invalid buffer index");
        if let Err(err) = packet
            .work
            .write(&self.mem, &header.as_bytes()[..header_size()])
        {
            tracelimit::warn_ratelimited!(
                error = &err as &dyn std::error::Error,
                "dropping RX packet after guest header write failure"
            );
            self.fail_memory_write(id);
        }
    }

    fn fail_capacity(&mut self, id: RxId, packet_len: usize, capacity: u32) {
        self.rx_capacity_drops += 1;
        self.rx_packets[id.0 as usize]
            .as_mut()
            .expect("invalid buffer index")
            .write_failed = true;
        tracelimit::warn_ratelimited!(
            packet_len,
            capacity,
            "dropping RX packet larger than guest buffer"
        );
    }

    fn fail_memory_write(&mut self, id: RxId) {
        self.rx_memory_write_drops += 1;
        self.rx_packets[id.0 as usize]
            .as_mut()
            .expect("invalid buffer index")
            .write_failed = true;
    }

    fn fail_packet(&mut self, ids: &[RxId]) {
        for id in ids {
            self.rx_packets[id.0 as usize]
                .as_mut()
                .expect("invalid buffer index")
                .write_failed = true;
        }
    }
}

impl BufferAccess for VirtioWorkPool {
    fn guest_memory(&self) -> &GuestMemory {
        &self.mem
    }

    fn write_data(&mut self, id: RxId, data: &[u8]) {
        let capacity = self.rx_packets[id.0 as usize]
            .as_ref()
            .expect("invalid buffer index")
            .cap;
        if data.len() > capacity as usize {
            self.fail_capacity(id, data.len(), capacity);
            return;
        }
        let result = self.rx_packets[id.0 as usize]
            .as_mut()
            .expect("invalid buffer index")
            .work
            .write_at_offset(header_size() as u64, &self.mem, data);
        if let Err(err) = result {
            tracelimit::warn_ratelimited!(
                len = data.len(),
                error = &err as &dyn std::error::Error,
                "dropping RX packet after guest memory write failure"
            );
            self.fail_memory_write(id);
        }
    }

    fn write_packet(&mut self, id: RxId, metadata: &RxMetadata, data: &[u8]) {
        if metadata.len != data.len() {
            self.rx_incomplete_drops += 1;
            self.rx_packets[id.0 as usize]
                .as_mut()
                .expect("invalid buffer index")
                .write_failed = true;
            tracelimit::warn_ratelimited!(
                metadata_len = metadata.len,
                data_len = data.len(),
                "dropping RX packet with inconsistent metadata length"
            );
            return;
        }
        self.write_data(id, data);
        self.write_header(id, metadata);
    }

    fn write_packet_segments(&mut self, id: RxId, metadata: &RxMetadata, segments: &[&[u8]]) {
        let total_len = segments.iter().map(|segment| segment.len()).sum::<usize>();
        if metadata.len != total_len {
            self.rx_incomplete_drops += 1;
            self.rx_packets[id.0 as usize]
                .as_mut()
                .expect("invalid buffer index")
                .write_failed = true;
            tracelimit::warn_ratelimited!(
                metadata_len = metadata.len,
                data_len = total_len,
                "dropping RX packet with inconsistent segment length"
            );
            return;
        }
        let capacity = self.rx_packets[id.0 as usize]
            .as_ref()
            .expect("invalid buffer index")
            .cap;
        if total_len > capacity as usize {
            self.fail_capacity(id, total_len, capacity);
            return;
        }

        let mut offset = header_size() as u64;
        for segment in segments {
            let result = self.rx_packets[id.0 as usize]
                .as_mut()
                .expect("invalid buffer index")
                .work
                .write_at_offset(offset, &self.mem, segment);
            if let Err(err) = result {
                tracelimit::warn_ratelimited!(
                    len = segment.len(),
                    error = &err as &dyn std::error::Error,
                    "dropping RX packet after guest memory write failure"
                );
                self.fail_memory_write(id);
                return;
            }
            offset += segment.len() as u64;
        }
        self.write_header(id, metadata);
    }

    fn write_packet_across_multiple_buffers(
        &mut self,
        ids: &[RxId],
        metadata: &RxMetadata,
        segments: &[&[u8]],
    ) -> bool {
        let Some(buffer_count) = u16::try_from(ids.len())
            .ok()
            .and_then(NonZeroU16::new)
            .filter(|count| self.mergeable_rx_buffers || count.get() == 1)
        else {
            tracelimit::warn_ratelimited!(
                count = ids.len(),
                "dropping RX packet: invalid receive buffer count"
            );
            return false;
        };
        if let [id] = ids {
            self.write_packet_segments(*id, metadata, segments);
            return true;
        }
        let packet_len: usize = segments.iter().map(|segment| segment.len()).sum();
        let total_capacity: usize = ids
            .iter()
            .enumerate()
            .map(|(index, &id)| self.packet_buffer_capacity(id, index) as usize)
            .sum();
        let last_capacity =
            self.packet_buffer_capacity(*ids.last().unwrap(), ids.len() - 1) as usize;
        if metadata.offset != 0
            || packet_len != metadata.len
            || packet_len > total_capacity
            || packet_len <= total_capacity - last_capacity
        {
            tracelimit::warn_ratelimited!(
                packet_len,
                total_capacity,
                "dropping RX packet: inconsistent multi-buffer receive"
            );
            return false;
        }

        let mut remaining = packet_len;
        for (buffer_index, &id) in ids.iter().enumerate() {
            let data_offset = if buffer_index == 0 { header_size() } else { 0 };
            let capacity = self.packet_buffer_capacity(id, buffer_index) as usize;
            let written = capacity.min(remaining);
            remaining -= written;
            let packet = self.rx_packets[id.0 as usize]
                .as_mut()
                .expect("invalid buffer index");
            packet.data_offset = data_offset as u32;
            packet.used_len = Some((data_offset + written) as u32);
        }
        let mut ranges = std::mem::take(&mut self.rx_write_ranges);
        for &id in ids {
            self.push_guest_addresses(id, &mut ranges);
        }
        let result = write_guest_segments(&self.mem, &ranges, segments);
        ranges.clear();
        self.rx_write_ranges = ranges;
        if let Err(err) = result {
            tracelimit::warn_ratelimited!(
                len = packet_len,
                error = &err as &dyn std::error::Error,
                "dropping RX packet after guest memory write failure"
            );
            self.fail_memory_write(ids[0]);
            self.fail_packet(ids);
            return true;
        }
        self.write_rx_header(ids[0], metadata, buffer_count);
        if self.rx_packets[ids[0].0 as usize]
            .as_ref()
            .unwrap()
            .write_failed
        {
            self.fail_packet(ids);
        }

        true
    }

    fn push_guest_addresses(&self, id: RxId, buf: &mut Vec<RxBufferSegment>) {
        let packet = self.rx_packets[id.0 as usize]
            .as_ref()
            .expect("invalid buffer index");
        if packet.write_failed {
            return;
        }
        let mut skip = packet.data_offset;
        let mut remaining = packet
            .used_len
            .map(|len| len - packet.data_offset)
            .unwrap_or(packet.cap);
        for p in packet.work.payload.iter().filter(|p| p.writeable) {
            let offset = skip.min(p.length);
            skip -= offset;
            let len = (p.length - offset).min(remaining);
            if len != 0 {
                buf.push(RxBufferSegment {
                    gpa: p.address.saturating_add(u64::from(offset)),
                    len,
                });
                remaining -= len;
            }
            if remaining == 0 {
                break;
            }
        }
    }

    fn capacity(&self, id: RxId) -> u32 {
        self.rx_packets[id.0 as usize]
            .as_ref()
            .expect("invalid buffer index")
            .cap
    }

    fn supports_multiple_buffers(&self) -> bool {
        self.mergeable_rx_buffers
    }

    fn packet_buffer_capacity(&self, id: RxId, buffer_index: usize) -> u32 {
        let capacity = self.capacity(id);
        if buffer_index == 0 {
            capacity
        } else if self.mergeable_rx_buffers {
            capacity + header_size() as u32
        } else {
            0
        }
    }

    fn write_header(&mut self, id: RxId, metadata: &RxMetadata) {
        assert_eq!(metadata.offset, 0);
        assert!(metadata.len > 0);

        let packet = self.rx_packets[id.0 as usize]
            .as_ref()
            .expect("invalid buffer index");
        if packet.write_failed {
            return;
        }
        if metadata.len > packet.cap as usize {
            let capacity = packet.cap;
            self.fail_capacity(id, metadata.len, capacity);
            return;
        }

        self.write_rx_header(id, metadata, NonZeroU16::MIN);
        self.rx_packets[id.0 as usize]
            .as_mut()
            .expect("invalid buffer index")
            .used_len = Some(metadata.len as u32 + header_size() as u32);
    }
}

fn write_guest_segments(
    mem: &GuestMemory,
    ranges: &[RxBufferSegment],
    segments: &[&[u8]],
) -> Result<(), guestmem::GuestMemoryError> {
    let mut ranges = ranges.iter().peekable();
    let mut segment_index = 0;
    let mut segment_offset = 0;
    while let Some(range) = ranges.next() {
        let mut gpa = range.gpa;
        let mut len = range.len as usize;
        // Guest buffers can be adjacent page fragments. Copy contiguous runs
        // together rather than restarting the copy for each virtio buffer.
        while let Some(next) = ranges.peek() {
            if gpa.checked_add(len as u64) != Some(next.gpa) {
                break;
            }
            len += next.len as usize;
            ranges.next();
        }
        while len != 0 {
            let segment = &segments[segment_index][segment_offset..];
            let write_len = segment.len().min(len);
            mem.write_at(gpa, &segment[..write_len])?;
            gpa = gpa.saturating_add(write_len as u64);
            len -= write_len;
            segment_offset += write_len;
            if segment_offset == segments[segment_index].len() {
                segment_index += 1;
                segment_offset = 0;
            }
        }
    }
    Ok(())
}
