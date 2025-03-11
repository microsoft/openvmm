// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::header_size;
use crate::VirtioNetHeader;
use guestmem::GuestMemory;
use net_backend::BufferAccess;
use net_backend::RxBufferSegment;
use net_backend::RxId;
use net_backend::RxMetadata;
use parking_lot::Mutex;
use std::sync::Arc;
use virtio::VirtioQueueCallbackWork;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

#[derive(Default)]
struct RxPacket {
    work: Option<VirtioQueueCallbackWork>,
    len: u32,
}

/// Holds virtio buffers available for a network backend to send data to the client.
#[derive(Clone)]
pub struct VirtioWorkPool {
    mem: GuestMemory,
    rx_packets: Arc<Vec<Mutex<RxPacket>>>,
    buffer_segments: Vec<RxBufferSegment>,
}

impl VirtioWorkPool {
    /// Create a new instance.
    pub fn new(mem: GuestMemory, queue_size: u16) -> Self {
        Self {
            mem,
            rx_packets: Arc::new(
                (0..queue_size)
                    .map(|_| Mutex::new(RxPacket::default()))
                    .collect(),
            ),
            buffer_segments: Vec::new(),
        }
    }

    /// Return a vector of RxIds currently available for use.
    pub fn ready(&self) -> Vec<RxId> {
        self.rx_packets
            .iter()
            .enumerate()
            .filter_map(|(i, e)| e.lock().work.as_ref().map(|_| RxId(i as u32)))
            .collect::<Vec<RxId>>()
    }

    /// Add a virtio work instance to the buffers available for use.
    pub fn queue_work(&self, work: VirtioQueueCallbackWork) -> RxId {
        let idx = work.descriptor_index();
        let mut packet = self.rx_packets[idx as usize].lock();
        assert!(packet.work.is_none());
        packet.work = Some(work);
        packet.len = 0;
        RxId(idx.into())
    }

    /// Notify the client that a receive packet is ready (network packet available).
    pub fn complete_packet(&self, rx_id: RxId) {
        let mut packet = self.rx_packets[rx_id.0 as usize].lock();
        let mut work = packet.work.take().expect("valid packet index");
        work.complete(packet.len);
    }
}

impl BufferAccess for VirtioWorkPool {
    fn guest_memory(&self) -> &GuestMemory {
        &self.mem
    }

    fn write_data(&mut self, id: RxId, data: &[u8]) {
        let mut locked_packet = self.rx_packets[id.0 as usize].lock();
        let work = locked_packet.work.as_ref().expect("invalid buffer index");
        if let Err(err) = work.write_at_offset(header_size() as u64, &self.mem, data) {
            tracing::warn!(
                len = data.len(),
                error = &err as &dyn std::error::Error,
                "rx memory write failure"
            );
        }
        locked_packet.len = (header_size() + data.len()) as u32;
    }

    fn guest_addresses(&mut self, id: RxId) -> &[RxBufferSegment] {
        let locked_packet = self.rx_packets[id.0 as usize].lock();
        let work = locked_packet.work.as_ref().expect("invalid buffer index");
        self.buffer_segments = work
            .payload
            .iter()
            .filter(|x| x.writeable)
            .map(|p| RxBufferSegment {
                gpa: p.address,
                len: p.length,
            })
            .collect();

        &self.buffer_segments
    }

    fn capacity(&self, id: RxId) -> u32 {
        let locked_packet = self.rx_packets[id.0 as usize].lock();
        let work = locked_packet.work.as_ref().expect("invalid buffer index");
        work.get_payload_length(true) as u32
    }

    fn write_header(&mut self, id: RxId, metadata: &RxMetadata) {
        assert_eq!(metadata.offset, 0);
        assert!(metadata.len > 0);

        // let flags = if let RxChecksumState::Good = metadata.ip_checksum {
        //     VirtioNetHeaderFlags::VIRTIO_NET_HDR_F_DATA_VALID.bits()
        // } else {
        //     0
        // };

        let virtio_net_header = VirtioNetHeader {
            num_buffers: 1,
            ..FromZeros::new_zeroed()
        };
        let locked_packet = self.rx_packets[id.0 as usize].lock();
        let work = locked_packet.work.as_ref().expect("invalid buffer index");
        assert_eq!(metadata.len + header_size(), locked_packet.len as usize);
        if let Err(err) = work.write(&self.mem, &virtio_net_header.as_bytes()[..header_size()]) {
            tracing::warn!(
                error = &err as &dyn std::error::Error,
                "failure writing header"
            );
        }
    }
}
