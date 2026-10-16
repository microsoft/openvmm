// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::VirtioNetHeader;
use crate::VirtioNetHeaderFlags;
use crate::header_size;
use guestmem::GuestMemory;
use inspect::Inspect;
use net_backend::BufferAccess;
use net_backend::RxBufferSegment;
use net_backend::RxId;
use net_backend::RxMetadata;
use std::cell::Cell;
use thiserror::Error;
use virtio::VirtioQueueCallbackWork;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

struct RxPacket {
    work: VirtioQueueCallbackWork,
    len: u32,
    cap: u32,
    id: RxId,
    slot: Option<usize>,
    written: Option<usize>,
    addresses_exposed: Cell<bool>,
    failed: bool,
    guest_write_failed: bool,
}

pub(crate) const OWNED_SLOTS: usize = 32;
pub(crate) const MAX_PACKET_SIZE: usize = 64 * 1024;
pub(crate) const MAX_PACKET_DESCRIPTORS: usize = 256;
// Include the virtio header and round up to a page. RX and TX use disjoint slots.
const SLOT_SIZE: usize = (MAX_PACKET_SIZE + header_size() + 4095) & !4095;

pub(crate) fn packet_buffer(len: usize, limit: usize) -> std::io::Result<Vec<u8>> {
    if len > limit {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    Ok(vec![0; len])
}

/// Fixed packet storage for one queue pair, with separate RX and TX slots.
struct OwnedArena {
    mem: GuestMemory,
    busy: [bool; OWNED_SLOTS * 2],
    tx_ids: [Option<u32>; OWNED_SLOTS],
    initialized: [Cell<usize>; OWNED_SLOTS * 2],
}

impl OwnedArena {
    fn allocate(&mut self, tx: bool) -> Option<usize> {
        let start = if tx { OWNED_SLOTS } else { 0 };
        let slot = (start..start + OWNED_SLOTS).find(|&i| !self.busy[i])?;
        self.busy[slot] = true;
        Some(slot)
    }

    fn release(&mut self, slot: usize) -> Result<(), guestmem::GuestMemoryError> {
        self.mem
            .fill_at(Self::address(slot), 0, self.initialized[slot].get())?;
        self.initialized[slot].set(0);
        self.busy[slot] = false;
        if slot >= OWNED_SLOTS {
            self.tx_ids[slot - OWNED_SLOTS] = None;
        }
        Ok(())
    }

    fn initialized(&self, slot: usize, len: usize) {
        self.initialized[slot].set(self.initialized[slot].get().max(len));
    }

    fn address(slot: usize) -> u64 {
        (slot * SLOT_SIZE) as u64
    }
}

/// Holds virtio buffers available for a network backend to send data to the client.
#[derive(Inspect)]
#[inspect(extra = "Self::inspect_extra")]
pub struct VirtioWorkPool {
    mem: GuestMemory,
    #[inspect(skip)]
    rx_packets: Vec<Option<RxPacket>>,
    #[inspect(skip)]
    owned: Option<OwnedArena>,
    /// Backend packet tokens are independent of guest descriptor indices.
    #[inspect(skip)]
    next_id: u32,
    #[inspect(skip)]
    failed: Cell<bool>,
    #[inspect(skip)]
    invalid_id: Cell<Option<u32>>,
}

#[derive(Debug, Error)]
pub enum RxCompletionError {
    #[error("invalid or stale backend RX ID {0}")]
    InvalidId(u32),
    #[error("RX buffer tracking failed")]
    TrackingFailed,
    #[error("RX packet data or metadata invalid")]
    InvalidPacket,
    #[error("RX metadata missing")]
    MissingMetadata,
    #[error("owned RX memory access failed")]
    Memory(#[from] guestmem::GuestMemoryError),
    #[error("RX guest copy failed")]
    GuestWrite(#[from] virtio::VirtioWriteError),
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
        );
    }

    /// Create a new instance.
    pub fn new(mem: GuestMemory, queue_size: u16) -> Self {
        Self {
            mem,
            rx_packets: (0..queue_size).map(|_| None).collect(),
            owned: None,
            next_id: 0,
            failed: Cell::new(false),
            invalid_id: Cell::new(None),
        }
    }

    /// Isolates backend access in a fixed 4.25 MiB arena per queue pair.
    /// There are 32 RX slots and 32 TX slots, independent of guest queue size.
    /// Full slots stop descriptor consumption until backend completion frees
    /// space. Temporary snapshots and descriptor tracking are additional.
    pub fn enable_owned(&mut self) {
        self.owned = Some(OwnedArena {
            mem: GuestMemory::allocate(SLOT_SIZE * OWNED_SLOTS * 2),
            busy: [false; OWNED_SLOTS * 2],
            tx_ids: [None; OWNED_SLOTS],
            initialized: [const { Cell::new(0) }; OWNED_SLOTS * 2],
        });
    }

    pub fn is_owned(&self) -> bool {
        self.owned.is_some()
    }

    pub fn has_room(&self, tx: bool) -> bool {
        self.owned.as_ref().is_none_or(|arena| {
            let start = if tx { OWNED_SLOTS } else { 0 };
            arena.busy[start..start + OWNED_SLOTS].iter().any(|x| !x)
        })
    }

    pub fn check_error(&self) -> Result<(), RxCompletionError> {
        if let Some(id) = self.invalid_id.get() {
            return Err(RxCompletionError::InvalidId(id));
        }
        if self.failed.get() {
            return Err(RxCompletionError::TrackingFailed);
        }
        Ok(())
    }

    fn new_backend_id(&mut self) -> u32 {
        // At most 64 owned slots are live, so wrapping always finds a free ID.
        loop {
            let id = self.next_id;
            self.next_id = id.wrapping_add(1);
            if !self
                .rx_packets
                .iter()
                .flatten()
                .any(|packet| packet.id.0 == id)
                && self
                    .owned
                    .as_ref()
                    .is_none_or(|arena| !arena.tx_ids.contains(&Some(id)))
            {
                return id;
            }
        }
    }

    #[cfg(test)]
    pub fn set_next_backend_id(&mut self, id: u32) {
        self.next_id = id;
    }

    pub fn stage_tx(&mut self, snapshot: &[u8]) -> anyhow::Result<(usize, u64, u32)> {
        let id = self.new_backend_id();
        let arena = self
            .owned
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no owned arena"))?;
        anyhow::ensure!(
            snapshot.len() <= MAX_PACKET_SIZE + header_size(),
            "TX packet too large"
        );
        let slot = arena
            .allocate(true)
            .ok_or_else(|| anyhow::anyhow!("owned TX arena full"))?;
        arena.tx_ids[slot - OWNED_SLOTS] = Some(id);
        let address = OwnedArena::address(slot);
        arena.initialized(slot, snapshot.len());
        if let Err(err) = arena.mem.write_at(address, snapshot) {
            arena.release(slot)?;
            return Err(err.into());
        }
        Ok((slot, address + header_size() as u64, id))
    }

    pub fn release_tx(&mut self, slot: usize) -> anyhow::Result<()> {
        self.owned
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no owned arena"))?
            .release(slot)?;
        Ok(())
    }

    fn packet(&self, id: RxId) -> Option<&RxPacket> {
        let packet = if self.is_owned() {
            self.rx_packets.iter().flatten().find(|p| p.id.0 == id.0)
        } else {
            self.rx_packets.get(id.0 as usize).and_then(Option::as_ref)
        };
        if packet.is_none() {
            self.failed.set(true);
            self.invalid_id.set(Some(id.0));
        }
        packet
    }

    /// Returns guest RAM for frontend descriptor access and staging copies.
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
                .filter_map(|e| e.as_ref().map(|p| p.id)),
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
        if self
            .rx_packets
            .get(idx as usize)
            .is_none_or(Option::is_some)
        {
            tracelimit::warn_ratelimited!("dropping RX buffer: descriptor index already in use");
            return Err(RxQueueError::DuplicateIndex(work));
        }
        if self.is_owned() && work.payload.len() > MAX_PACKET_DESCRIPTORS {
            return Err(RxQueueError::TooSmall(work));
        }
        let payload_length = work
            .payload
            .iter()
            .filter(|p| p.writeable)
            .try_fold(0u32, |len, p| len.checked_add(p.length));
        let Some(payload_length) = payload_length else {
            return Err(RxQueueError::TooSmall(work));
        };
        let Some(cap) = payload_length.checked_sub(header_size() as u32) else {
            tracelimit::warn_ratelimited!(
                len = payload_length,
                "dropping RX buffer: payload length smaller than virtio-net header size"
            );
            return Err(RxQueueError::TooSmall(work));
        };
        let (id, slot, cap) = if self.is_owned() {
            let id = self.new_backend_id();
            let Some(slot) = self.owned.as_mut().and_then(|arena| arena.allocate(false)) else {
                self.failed.set(true);
                return Err(RxQueueError::TooSmall(work));
            };
            (RxId(id), Some(slot), cap.min(MAX_PACKET_SIZE as u32))
        } else {
            (RxId(idx.into()), None, cap)
        };
        self.rx_packets[idx as usize] = Some(RxPacket {
            len: 0,
            cap,
            work,
            id,
            slot,
            written: None,
            addresses_exposed: Cell::new(false),
            failed: false,
            guest_write_failed: false,
        });
        Ok(id)
    }

    /// Take the RX work item for the given packet, returning it with the
    /// computed payload length. The caller is responsible for completing
    /// the descriptor via the queue.
    #[must_use = "caller must complete the returned work via VirtioQueue::complete"]
    pub fn take_rx_work(
        &mut self,
        rx_id: RxId,
    ) -> Result<(VirtioQueueCallbackWork, u32), RxCompletionError> {
        self.check_error()?;
        let idx = self
            .packet(rx_id)
            .ok_or(RxCompletionError::InvalidId(rx_id.0))?
            .work
            .descriptor_index() as usize;
        let packet = self.rx_packets[idx]
            .take()
            .ok_or(RxCompletionError::InvalidId(rx_id.0))?;
        if packet.failed {
            return Err(RxCompletionError::InvalidPacket);
        }
        let payload_len = if packet.len == 0 || packet.guest_write_failed {
            // No successful header/data copy can be published.
            tracelimit::warn_ratelimited!("dropping RX buffer: packet not written");
            0
        } else {
            packet.len + header_size() as u32
        };
        if let Some(slot) = packet.slot {
            let arena = self
                .owned
                .as_mut()
                .ok_or(RxCompletionError::TrackingFailed)?;
            let result = if payload_len == 0 {
                Err(RxCompletionError::MissingMetadata)
            } else {
                packet_buffer(payload_len as usize, MAX_PACKET_SIZE + header_size())
                    .map_err(|_| RxCompletionError::InvalidPacket)
                    .and_then(|mut bytes| {
                        arena
                            .mem
                            .read_at(OwnedArena::address(slot), &mut bytes)
                            .map_err(RxCompletionError::Memory)?;
                        packet
                            .work
                            .write(&self.mem, &bytes)
                            .map_err(RxCompletionError::GuestWrite)
                    })
            };
            arena.release(slot)?;
            result?;
        }
        Ok((packet.work, payload_len))
    }

    /// Repost outstanding RX snapshots with fresh IDs only after the old queue
    /// owners are dropped and Endpoint::stop has finished.
    pub fn rearm_owned_rx(&mut self) -> anyhow::Result<()> {
        self.check_error()?;
        for idx in 0..self.rx_packets.len() {
            if self.rx_packets[idx].is_none() {
                continue;
            }
            let id = RxId(self.new_backend_id());
            let packet = self.rx_packets[idx].as_mut().unwrap();
            let slot = packet
                .slot
                .ok_or_else(|| anyhow::anyhow!("RX staging slot missing"))?;
            let arena = self
                .owned
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("no owned arena"))?;
            arena.release(slot)?;
            arena.busy[slot] = true;
            packet.id = id;
            packet.len = 0;
            packet.written = None;
            packet.addresses_exposed.set(false);
            packet.failed = false;
            packet.guest_write_failed = false;
        }
        Ok(())
    }

    pub fn discard_rx(&mut self, id: RxId) -> anyhow::Result<()> {
        let idx = self
            .packet(id)
            .ok_or_else(|| anyhow::anyhow!("invalid RX drain ID"))?
            .work
            .descriptor_index() as usize;
        let packet = self.rx_packets[idx]
            .take()
            .ok_or_else(|| anyhow::anyhow!("duplicate RX drain ID"))?;
        if let Some(slot) = packet.slot {
            self.owned
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("no owned arena"))?
                .release(slot)?;
        }
        Ok(())
    }
}

impl BufferAccess for VirtioWorkPool {
    fn guest_memory(&self) -> &GuestMemory {
        self.owned.as_ref().map_or(&self.mem, |arena| &arena.mem)
    }

    fn write_data(&mut self, id: RxId, data: &[u8]) {
        let Some(idx) = self.packet(id).map(|p| p.work.descriptor_index() as usize) else {
            return;
        };
        let packet = self.rx_packets[idx].as_mut().unwrap();
        if data.len() > packet.cap as usize {
            packet.failed = true;
            return;
        }
        if let Some(slot) = packet.slot {
            let arena = self.owned.as_ref().unwrap();
            arena.initialized(slot, header_size() + data.len());
            packet.failed |= arena
                .mem
                .write_at(OwnedArena::address(slot) + header_size() as u64, data)
                .is_err();
            packet.written = Some(data.len());
            return;
        }
        if let Err(err) = packet
            .work
            .write_at_offset(header_size() as u64, &self.mem, data)
        {
            packet.guest_write_failed = true;
            tracelimit::warn_ratelimited!(
                len = data.len(),
                error = &err as &dyn std::error::Error,
                "rx memory write failure"
            );
        }
    }

    fn write_packet_segments(&mut self, id: RxId, metadata: &RxMetadata, segments: &[&[u8]]) {
        let Some(idx) = self.packet(id).map(|p| p.work.descriptor_index() as usize) else {
            return;
        };
        let total = segments
            .iter()
            .try_fold(0usize, |n, s| n.checked_add(s.len()));
        let packet = self.rx_packets[idx].as_mut().unwrap();
        if total.is_none_or(|n| n > packet.cap as usize) {
            packet.failed = true;
            return;
        }
        if self.owned.is_some() {
            let total = total.unwrap();
            let mut bytes = match packet_buffer(total, MAX_PACKET_SIZE) {
                Ok(bytes) => bytes,
                Err(err) => {
                    packet.failed = true;
                    tracelimit::warn_ratelimited!(
                        len = total,
                        error = &err as &dyn std::error::Error,
                        "owned RX packet exceeds staging limit"
                    );
                    return;
                }
            };
            let mut offset = 0;
            for segment in segments {
                let end = offset + segment.len();
                bytes[offset..end].copy_from_slice(segment);
                offset = end;
            }
            self.write_data(id, &bytes);
            self.write_header(id, metadata);
            return;
        }
        let mut offset = header_size() as u64;
        for segment in segments {
            if let Err(err) = packet.work.write_at_offset(offset, &self.mem, segment) {
                packet.guest_write_failed = true;
                tracelimit::warn_ratelimited!(
                    len = segment.len(),
                    error = &err as &dyn std::error::Error,
                    "rx memory write failure"
                );
            }
            offset += segment.len() as u64;
        }
        self.write_header(id, metadata);
    }

    fn push_guest_addresses(&self, id: RxId, buf: &mut Vec<RxBufferSegment>) {
        let Some(packet) = self.packet(id) else {
            return;
        };
        if let Some(slot) = packet.slot {
            packet.addresses_exposed.set(true);
            self.owned
                .as_ref()
                .unwrap()
                .initialized(slot, header_size() + packet.cap as usize);
            buf.push(RxBufferSegment {
                gpa: OwnedArena::address(slot),
                len: packet.cap + header_size() as u32,
            });
            return;
        }
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
        self.packet(id).map_or(0, |p| p.cap)
    }

    fn write_header(&mut self, id: RxId, metadata: &RxMetadata) {
        let Some(idx) = self.packet(id).map(|p| p.work.descriptor_index() as usize) else {
            return;
        };
        let packet = self.rx_packets[idx].as_mut().unwrap();
        if metadata.offset != 0
            || metadata.len == 0
            || metadata.len > packet.cap as usize
            || packet.written.is_some_and(|len| len != metadata.len)
            || (packet.slot.is_some()
                && packet.written.is_none()
                && !packet.addresses_exposed.get())
        {
            packet.failed = true;
            return;
        }

        // Map RxMetadata checksum state to virtio-net header flags.
        // Set VIRTIO_NET_HDR_F_DATA_VALID when both IP and L4 checksums have
        // been validated (Good or ValidatedButWrong, e.g. after RSC/LRO),
        // telling the guest it can skip re-verification.
        let data_valid = metadata.ip_checksum.is_valid() && metadata.l4_checksum.is_valid();
        let flags = VirtioNetHeaderFlags::new().with_data_valid(data_valid);

        let virtio_net_header = VirtioNetHeader {
            flags: flags.into(),
            num_buffers: 1,
            ..FromZeros::new_zeroed()
        };
        if let Some(slot) = packet.slot {
            self.owned
                .as_ref()
                .unwrap()
                .initialized(slot, header_size());
            packet.failed |= self
                .owned
                .as_ref()
                .unwrap()
                .mem
                .write_at(
                    OwnedArena::address(slot),
                    &virtio_net_header.as_bytes()[..header_size()],
                )
                .is_err();
            packet.len = metadata.len as u32;
            return;
        }
        if let Err(err) = packet
            .work
            .write(&self.mem, &virtio_net_header.as_bytes()[..header_size()])
        {
            packet.guest_write_failed = true;
            tracelimit::warn_ratelimited!(
                error = &err as &dyn std::error::Error,
                "failure writing header"
            );
            return;
        }
        packet.len = metadata.len as u32;
    }
}
