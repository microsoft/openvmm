// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::VirtioNetHeader;
use crate::VirtioNetHeaderFlags;
use crate::header_size;
use guestmem::GuestMemory;
use inspect::Inspect;
use net_backend::BufferAccess;
use net_backend::RxBufferSegment;
use net_backend::RxGsoProtocol;
use net_backend::RxId;
use net_backend::RxMetadata;
use virtio::VirtioQueueCallbackWork;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

struct RxPacket {
    work: VirtioQueueCallbackWork,
    len: u32,
    cap: u32,
    write_failed: bool,
}

/// Holds virtio buffers available for a network backend to send data to the client.
#[derive(Inspect)]
#[inspect(extra = "Self::inspect_extra")]
pub struct VirtioWorkPool {
    mem: GuestMemory,
    #[inspect(skip)]
    rx_packets: Vec<Option<RxPacket>>,
    rx_capacity_drops: u64,
    rx_memory_write_drops: u64,
    rx_incomplete_drops: u64,
}

/// Reason a submitted RX buffer could not be queued to the backend, returned
/// with the original work item so the caller can decide how to handle it.
pub enum RxQueueError {
    /// The descriptor index is already in use (duplicate guest submission).
    /// This is a fatal protocol violation: the pool slot is already taken, so
    /// the buffer cannot be tracked.
    DuplicateIndex(VirtioQueueCallbackWork),
    /// The buffer is smaller than the virtio-net header. It must be completed
    /// (dropped) in avail order rather than posted to the backend.
    TooSmall(VirtioQueueCallbackWork),
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
    pub fn new(mem: GuestMemory, queue_size: u16) -> Self {
        Self {
            mem,
            rx_packets: (0..queue_size).map(|_| None).collect(),
            rx_capacity_drops: 0,
            rx_memory_write_drops: 0,
            rx_incomplete_drops: 0,
        }
    }

    /// Returns a reference to the guest memory.
    pub fn mem(&self) -> &GuestMemory {
        &self.mem
    }

    /// Fills `buf` with the RxIds of currently available buffers. `buf` must be
    /// at least as big as the virtio queue size, passed to `new()`.
    ///
    /// Returns the number of entries written.
    pub fn fill_ready(&self, buf: &mut [RxId]) -> usize {
        assert!(buf.len() >= self.rx_packets.len());
        let mut n = 0;
        for (dest, src) in buf.iter_mut().zip(
            self.rx_packets
                .iter()
                .enumerate()
                .filter_map(|(i, e)| e.is_some().then_some(RxId(i as u32))),
        ) {
            *dest = src;
            n += 1;
        }
        n
    }

    /// Add a virtio work instance to the buffers available for use.
    ///
    /// Returns `Err` with the work item if the buffer cannot be posted to the
    /// backend, distinguishing a fatal duplicate descriptor index from a
    /// too-small buffer that should be dropped in order.
    pub fn queue_work(&mut self, work: VirtioQueueCallbackWork) -> Result<RxId, RxQueueError> {
        let idx = work.descriptor_index();
        let packet = &mut self.rx_packets[idx as usize];
        if packet.is_some() {
            tracelimit::warn_ratelimited!("dropping RX buffer: descriptor index already in use");
            return Err(RxQueueError::DuplicateIndex(work));
        }
        let payload_length = work.get_payload_length(true) as u32;
        let Some(cap) = payload_length.checked_sub(header_size() as u32) else {
            tracelimit::warn_ratelimited!(
                len = payload_length,
                "dropping RX buffer: payload length smaller than virtio-net header size"
            );
            return Err(RxQueueError::TooSmall(work));
        };
        *packet = Some(RxPacket {
            len: 0,
            cap,
            write_failed: false,
            work,
        });
        Ok(RxId(idx.into()))
    }

    /// Take the RX work item for the given packet, returning it with the
    /// computed payload length. The caller is responsible for completing
    /// the descriptor via the queue.
    #[must_use = "caller must complete the returned work via VirtioQueue::complete"]
    pub fn take_rx_work(&mut self, rx_id: RxId) -> (VirtioQueueCallbackWork, u32, bool) {
        let packet = self.rx_packets[rx_id.0 as usize]
            .take()
            .expect("valid packet index");
        let dropped = packet.write_failed || packet.len == 0;
        let payload_len = if dropped {
            if !packet.write_failed {
                self.rx_incomplete_drops += 1;
                tracelimit::warn_ratelimited!("dropping RX buffer: packet write not completed");
            }
            0
        } else {
            packet.len + header_size() as u32
        };
        (packet.work, payload_len, dropped)
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

    fn push_guest_addresses(&self, id: RxId, buf: &mut Vec<RxBufferSegment>) {
        let packet = self.rx_packets[id.0 as usize]
            .as_ref()
            .expect("invalid buffer index");
        buf.extend(
            packet
                .work
                .payload
                .iter()
                .filter(|x| x.writeable)
                .map(|p| RxBufferSegment {
                    gpa: p.address,
                    len: p.length,
                }),
        );
    }

    fn capacity(&self, id: RxId) -> u32 {
        self.rx_packets[id.0 as usize]
            .as_ref()
            .expect("invalid buffer index")
            .cap
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

        // Map RxMetadata checksum state to virtio-net header flags.
        // Set VIRTIO_NET_HDR_F_DATA_VALID when both IP and L4 checksums have
        // been validated (Good or ValidatedButWrong, e.g. after RSC/LRO),
        // telling the guest it can skip re-verification.
        let data_valid = metadata.checksum_offload.is_none()
            && metadata.ip_checksum.is_valid()
            && metadata.l4_checksum.is_valid();
        let flags = VirtioNetHeaderFlags::new()
            .with_needs_csum(metadata.checksum_offload.is_some())
            .with_data_valid(data_valid);
        let (gso_type, hdr_len, gso_size) = if let Some(gso) = metadata.gso {
            let protocol = match gso.protocol {
                RxGsoProtocol::TcpV4 => crate::VirtioNetHeaderGsoProtocol::TCPV4,
                RxGsoProtocol::TcpV6 => crate::VirtioNetHeaderGsoProtocol::TCPV6,
            };
            (
                crate::VirtioNetHeaderGso::new()
                    .with_protocol(protocol)
                    .with_ecn(gso.ecn)
                    .into_bits(),
                gso.header_len,
                gso.max_segment_size,
            )
        } else {
            (0, 0, 0)
        };
        let (csum_start, csum_offset) = metadata
            .checksum_offload
            .map(|checksum| (checksum.start, checksum.offset))
            .unwrap_or_default();

        let virtio_net_header = VirtioNetHeader {
            flags: flags.into(),
            gso_type,
            hdr_len,
            gso_size,
            csum_start,
            csum_offset,
            num_buffers: 1,
            ..FromZeros::new_zeroed()
        };
        let result = self.rx_packets[id.0 as usize]
            .as_mut()
            .expect("invalid buffer index")
            .work
            .write(&self.mem, &virtio_net_header.as_bytes()[..header_size()]);
        if let Err(err) = result {
            tracelimit::warn_ratelimited!(
                len = header_size(),
                error = &err as &dyn std::error::Error,
                "dropping RX packet after guest header write failure"
            );
            self.fail_memory_write(id);
            return;
        }
        self.rx_packets[id.0 as usize]
            .as_mut()
            .expect("invalid buffer index")
            .len = metadata.len as u32;
    }
}
