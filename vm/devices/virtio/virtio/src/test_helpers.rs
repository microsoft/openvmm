// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared test helpers for writing integration tests that drive virtio devices
//! through descriptor rings.
//!
//! These helpers manipulate split virtqueue descriptor tables, available rings,
//! and used rings in guest memory — the same operations a guest driver would
//! perform.

use crate::DynVirtioDevice;
use crate::spec::VIRTIO_MMIO_INTERRUPT_STATUS_CONFIG_CHANGE as CONFIG_CHANGE;
use crate::spec::mmio::VirtioMmioRegister as Reg;
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
use crate::transport::VirtioMmioDevice;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use chipset_device::poll_device::PollDevice;
use core::mem::offset_of;
use guestmem::GuestMemory;
use pal_async::wait::PolledWait;
use pal_event::Event;
use std::future::poll_fn;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;
use vmcore::line_interrupt::LineInterrupt;
use vmcore::line_interrupt::test_helpers::TestLineInterruptTarget;

/// Check guest config notifications from a device with queues already running.
pub struct ConfigInterrupt {
    mmio: VirtioMmioDevice,
    irq: Arc<TestLineInterruptTarget>,
    generation: u8,
}

impl ConfigInterrupt {
    /// Attach an MMIO transport without starting any additional queues.
    pub async fn new(
        driver: &pal_async::DefaultDriver,
        device: impl DynVirtioDevice + 'static,
    ) -> Self {
        let irq = TestLineInterruptTarget::new_arc();
        let line = LineInterrupt::new_with_target("virtio config", irq.clone(), 0);
        let mut mmio = VirtioMmioDevice::new(
            Box::new(device),
            driver,
            GuestMemory::empty(),
            line,
            None,
            0,
            0x1000,
        )
        .unwrap();
        mmio.mmio_write(Reg::DRIVER_FEATURES_SEL.0.into(), &1u32.to_le_bytes())
            .unwrap();
        mmio.mmio_write(Reg::DRIVER_FEATURES.0.into(), &1u32.to_le_bytes())
            .unwrap();
        let status = 15u32; // ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK
        let IoResult::Defer(token) = mmio.mmio_write(Reg::STATUS.0.into(), &status.to_le_bytes())
        else {
            panic!("expected deferred DRIVER_OK");
        };
        poll_mmio(&mut mmio, token.write_future()).await.unwrap();
        let mut actual = [0; 4];
        mmio.mmio_read(Reg::STATUS.0.into(), &mut actual).unwrap();
        assert_eq!(u32::from_le_bytes(actual), status);
        let mut generation = [0];
        mmio.mmio_read(Reg::CONFIG_GENERATION.0.into(), &mut generation)
            .unwrap();
        Self {
            mmio,
            irq,
            generation: generation[0],
        }
    }

    /// Verify and acknowledge a config interrupt, then read device config.
    pub async fn next(&mut self, config_offset: u16) -> u32 {
        poll_mmio(&mut self.mmio, poll_fn(|cx| self.irq.poll_high(cx, 0))).await;
        let mut bytes = [0; 4];
        self.mmio
            .mmio_read(Reg::INTERRUPT_STATUS.0.into(), &mut bytes)
            .unwrap();
        assert_eq!(u32::from_le_bytes(bytes), CONFIG_CHANGE);
        self.generation = self.generation.wrapping_add(1);
        self.mmio
            .mmio_read(Reg::CONFIG_GENERATION.0.into(), &mut bytes)
            .unwrap();
        assert_eq!(u32::from_le_bytes(bytes), u32::from(self.generation));
        self.mmio
            .mmio_write(Reg::INTERRUPT_ACK.0.into(), &CONFIG_CHANGE.to_le_bytes())
            .unwrap();
        let IoResult::Defer(token) = self
            .mmio
            .mmio_read(u64::from(Reg::CONFIG.0 + config_offset), &mut bytes)
        else {
            panic!("expected deferred config read");
        };
        token.read_future(&mut bytes).await.unwrap();
        u32::from_le_bytes(bytes)
    }
}

async fn poll_mmio<T>(mmio: &mut VirtioMmioDevice, future: impl Future<Output = T>) -> T {
    let mut future = pin!(future);
    mesh::CancelContext::new()
        .with_timeout(Duration::from_secs(2))
        .until_cancelled(poll_fn(|cx| {
            mmio.poll_device(cx);
            future.as_mut().poll(cx)
        }))
        .await
        .expect("timed out polling virtio MMIO")
}

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
