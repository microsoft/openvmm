// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared test helpers for writing integration tests that drive virtio devices
//! through descriptor rings.
//!
//! These helpers manipulate split virtqueue descriptor tables, available rings,
//! and used rings in guest memory — the same operations a guest driver would
//! perform.

use crate::spec::queue::AVAIL_ELEMENT_SIZE;
use crate::spec::queue::AVAIL_OFFSET_FLAGS;
use crate::spec::queue::AVAIL_OFFSET_IDX;
use crate::spec::queue::AVAIL_OFFSET_RING;
use crate::spec::queue::DescriptorFlags;
use crate::spec::queue::SplitDescriptor;
use crate::spec::queue::USED_ELEMENT_SIZE;
use crate::spec::queue::USED_OFFSET_FLAGS;
use crate::spec::queue::USED_OFFSET_IDX;
use crate::spec::queue::USED_OFFSET_RING;
use crate::spec::queue::UsedElement;
use core::mem::offset_of;
use guestmem::GuestMemory;
use pal_async::wait::PolledWait;
use pal_event::Event;
use std::time::Duration;

/// Write a split virtio descriptor at the given descriptor table base.
pub fn write_descriptor(
    mem: &GuestMemory,
    desc_table_base: u64,
    index: u16,
    addr: u64,
    len: u32,
    flags: DescriptorFlags,
    next: u16,
) {
    let base = desc_table_base + size_of::<SplitDescriptor>() as u64 * index as u64;
    mem.write_at(
        base + offset_of!(SplitDescriptor, address) as u64,
        &addr.to_le_bytes(),
    )
    .unwrap();
    mem.write_at(
        base + offset_of!(SplitDescriptor, length) as u64,
        &len.to_le_bytes(),
    )
    .unwrap();
    mem.write_at(
        base + offset_of!(SplitDescriptor, flags_raw) as u64,
        &u16::from(flags).to_le_bytes(),
    )
    .unwrap();
    mem.write_at(
        base + offset_of!(SplitDescriptor, next) as u64,
        &next.to_le_bytes(),
    )
    .unwrap();
}

/// Initialize an avail ring (flags=0, idx=0).
pub fn init_avail_ring(mem: &GuestMemory, avail_addr: u64) {
    mem.write_at(avail_addr + AVAIL_OFFSET_FLAGS, &0u16.to_le_bytes())
        .unwrap();
    mem.write_at(avail_addr + AVAIL_OFFSET_IDX, &0u16.to_le_bytes())
        .unwrap();
}

/// Initialize a used ring (flags=0, idx=0).
pub fn init_used_ring(mem: &GuestMemory, used_addr: u64) {
    mem.write_at(used_addr + USED_OFFSET_FLAGS, &0u16.to_le_bytes())
        .unwrap();
    mem.write_at(used_addr + USED_OFFSET_IDX, &0u16.to_le_bytes())
        .unwrap();
}

/// Make a descriptor index available in the avail ring and bump the index.
pub fn make_available(
    mem: &GuestMemory,
    avail_addr: u64,
    queue_size: u16,
    desc_index: u16,
    avail_idx: &mut u16,
) {
    let ring_offset =
        avail_addr + AVAIL_OFFSET_RING + AVAIL_ELEMENT_SIZE * (*avail_idx % queue_size) as u64;
    mem.write_at(ring_offset, &desc_index.to_le_bytes())
        .unwrap();
    *avail_idx = avail_idx.wrapping_add(1);
    mem.write_at(avail_addr + AVAIL_OFFSET_IDX, &avail_idx.to_le_bytes())
        .unwrap();
}

/// Read the used ring index.
pub fn read_used_idx(mem: &GuestMemory, used_addr: u64) -> u16 {
    let mut buf = [0u8; 2];
    mem.read_at(used_addr + USED_OFFSET_IDX, &mut buf).unwrap();
    u16::from_le_bytes(buf)
}

/// Read a used ring entry (id, len) at the given ring index.
pub fn read_used_entry(
    mem: &GuestMemory,
    used_addr: u64,
    queue_size: u16,
    index: u16,
) -> (u32, u32) {
    let entry_offset =
        used_addr + USED_OFFSET_RING + USED_ELEMENT_SIZE * (index % queue_size) as u64;
    let mut id_buf = [0u8; 4];
    let mut len_buf = [0u8; 4];
    mem.read_at(
        entry_offset + offset_of!(UsedElement, id) as u64,
        &mut id_buf,
    )
    .unwrap();
    mem.read_at(
        entry_offset + offset_of!(UsedElement, len) as u64,
        &mut len_buf,
    )
    .unwrap();
    (u32::from_le_bytes(id_buf), u32::from_le_bytes(len_buf))
}

/// Read the next used ring entry, returning `(desc_id, bytes_written)` or
/// `None` if no new entries are available.
///
/// Advances `*used_idx` when an entry is consumed.
pub fn read_used(
    mem: &GuestMemory,
    used_addr: u64,
    queue_size: u16,
    used_idx: &mut u16,
) -> Option<(u16, u32)> {
    let current_used_idx = read_used_idx(mem, used_addr);
    if current_used_idx == *used_idx {
        return None;
    }
    let (id, len) = read_used_entry(mem, used_addr, queue_size, *used_idx);
    *used_idx = used_idx.wrapping_add(1);
    Some((id as u16, len))
}

/// Wait for the next used ring entry, polling with a timeout.
///
/// `interrupt_event` is the event signaled by the device when it writes to
/// the used ring. This function polls until [`read_used`] returns `Some`,
/// panicking if 5 seconds elapse without an entry.
pub async fn wait_for_used(
    driver: &pal_async::DefaultDriver,
    interrupt_event: &Event,
    mem: &GuestMemory,
    used_addr: u64,
    queue_size: u16,
    used_idx: &mut u16,
) -> (u16, u32) {
    let mut wait = PolledWait::new(driver, interrupt_event.clone()).unwrap();
    mesh::CancelContext::new()
        .with_timeout(Duration::from_secs(5))
        .until_cancelled(async {
            loop {
                if let Some(entry) = read_used(mem, used_addr, queue_size, used_idx) {
                    return entry;
                }
                wait.wait().await.unwrap();
            }
        })
        .await
        .expect("timed out waiting for used ring entry")
}
