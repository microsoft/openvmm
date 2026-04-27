// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::VirtioNetHeader;
use crate::VirtioNetHeaderFlags;
use crate::VirtioNetHeaderGso;
use crate::VirtioNetHeaderGsoProtocol;
use crate::header_size;
use guestmem::GuestMemory;
use inspect::Inspect;
use net_backend::BufferAccess;
use net_backend::L3Protocol;
use net_backend::RxBufferSegment;
use net_backend::RxId;
use net_backend::RxMetadata;
use virtio::VirtioQueueCallbackWork;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

struct RxPacket {
    work: VirtioQueueCallbackWork,
    len: u32,
    cap: u32,
}

/// Holds virtio buffers available for a network backend to send data to the client.
#[derive(Inspect)]
#[inspect(extra = "Self::inspect_extra")]
pub struct VirtioWorkPool {
    mem: GuestMemory,
    #[inspect(skip)]
    rx_packets: Vec<Option<RxPacket>>,
    /// Whether the guest negotiated VIRTIO_NET_F_GUEST_TSO4.
    guest_tso4: bool,
    /// Whether the guest negotiated VIRTIO_NET_F_GUEST_TSO6.
    guest_tso6: bool,
}

impl VirtioWorkPool {
    fn inspect_extra(&self, resp: &mut inspect::Response<'_>) {
        resp.field(
            "pending_rx_packets",
            self.rx_packets.iter().filter(|p| p.is_some()).count(),
        );
    }

    /// Create a new instance.
    pub fn new(mem: GuestMemory, queue_size: u16, guest_tso4: bool, guest_tso6: bool) -> Self {
        Self {
            mem,
            rx_packets: (0..queue_size).map(|_| None).collect(),
            guest_tso4,
            guest_tso6,
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
    /// Returns `Err` with the work item if the descriptor index is already in
    /// use (duplicate submission by the guest).
    pub fn queue_work(
        &mut self,
        work: VirtioQueueCallbackWork,
    ) -> Result<RxId, VirtioQueueCallbackWork> {
        let idx = work.descriptor_index();
        let packet = &mut self.rx_packets[idx as usize];
        if packet.is_some() {
            tracelimit::warn_ratelimited!("dropping RX buffer: descriptor index already in use");
            return Err(work);
        }
        let payload_length = work.get_payload_length(true) as u32;
        let Some(cap) = payload_length.checked_sub(header_size() as u32) else {
            tracelimit::warn_ratelimited!(
                len = payload_length,
                "dropping RX buffer: payload length smaller than virtio-net header size"
            );
            return Err(work);
        };
        *packet = Some(RxPacket { len: 0, cap, work });
        Ok(RxId(idx.into()))
    }

    /// Take the RX work item for the given packet, returning it with the
    /// computed payload length. The caller is responsible for completing
    /// the descriptor via the queue.
    #[must_use = "caller must complete the returned work via VirtioQueue::complete"]
    pub fn take_rx_work(&mut self, rx_id: RxId) -> (VirtioQueueCallbackWork, u32) {
        let packet = self.rx_packets[rx_id.0 as usize]
            .take()
            .expect("valid packet index");
        let payload_len = if packet.len == 0 {
            // Header was not written, so treat as empty packet.
            tracelimit::warn_ratelimited!("dropping RX buffer: header not written");
            0
        } else {
            packet.len + header_size() as u32
        };
        (packet.work, payload_len)
    }
}

impl BufferAccess for VirtioWorkPool {
    fn guest_memory(&self) -> &GuestMemory {
        &self.mem
    }

    fn write_data(&mut self, id: RxId, data: &[u8]) {
        let packet = self.rx_packets[id.0 as usize]
            .as_mut()
            .expect("invalid buffer index");
        if let Err(err) = packet
            .work
            .write_at_offset(header_size() as u64, &self.mem, data)
        {
            tracelimit::warn_ratelimited!(
                len = data.len(),
                error = &err as &dyn std::error::Error,
                "rx memory write failure"
            );
        }
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

        // Map RxMetadata checksum state to virtio-net header flags.
        // Set VIRTIO_NET_HDR_F_DATA_VALID when both IP and L4 checksums have
        // been validated (Good or ValidatedButWrong, e.g. after RSC/LRO),
        // telling the guest it can skip re-verification.
        let data_valid = metadata.ip_checksum.is_valid() && metadata.l4_checksum.is_valid();
        let flags = VirtioNetHeaderFlags::new().with_data_valid(data_valid);

        // Build GSO fields when the backend indicates a large/coalesced packet
        // and the guest has negotiated the corresponding GUEST_TSO feature.
        let gso_allowed = match metadata.l3_protocol {
            L3Protocol::Ipv4 => self.guest_tso4,
            L3Protocol::Ipv6 => self.guest_tso6,
            L3Protocol::Unknown => false,
        };
        let (gso_type, gso_size, hdr_len, csum_start, csum_offset) =
            if metadata.gso_size > 0 && metadata.l2_len > 0 && metadata.l3_len > 0 && gso_allowed {
                let gso_protocol = match metadata.l3_protocol {
                    L3Protocol::Ipv4 => VirtioNetHeaderGsoProtocol::TCPV4,
                    L3Protocol::Ipv6 => VirtioNetHeaderGsoProtocol::TCPV6,
                    L3Protocol::Unknown => VirtioNetHeaderGsoProtocol::NONE,
                };
                let gso_type_byte: u8 =
                    VirtioNetHeaderGso::new().with_protocol(gso_protocol).into();
                let total_hdr = metadata.l2_len as u16 + metadata.l3_len + metadata.l4_len as u16;
                let csum_start = metadata.l2_len as u16 + metadata.l3_len;
                // TCP checksum offset within TCP header is 16.
                let csum_offset: u16 = 16;
                (
                    gso_type_byte,
                    metadata.gso_size,
                    total_hdr,
                    csum_start,
                    csum_offset,
                )
            } else {
                (0, 0, 0, 0, 0)
            };

        // When GSO is active, set NEEDS_CSUM so the guest computes
        // per-segment checksums.
        let flags = if gso_size > 0 {
            flags.with_needs_csum(true)
        } else {
            flags
        };

        let virtio_net_header = VirtioNetHeader {
            flags: flags.into(),
            gso_type,
            gso_size,
            hdr_len,
            csum_start,
            csum_offset,
            num_buffers: 1,
            ..FromZeros::new_zeroed()
        };
        let packet = self.rx_packets[id.0 as usize]
            .as_mut()
            .expect("invalid buffer index");
        if let Err(err) = packet
            .work
            .write(&self.mem, &virtio_net_header.as_bytes()[..header_size()])
        {
            tracelimit::warn_ratelimited!(
                error = &err as &dyn std::error::Error,
                "failure writing header"
            );
            return;
        }
        assert!(
            metadata.len <= packet.cap as usize,
            "packet len {} exceeds buffer capacity {}",
            metadata.len,
            packet.cap
        );
        packet.len = metadata.len as u32;
    }
}
