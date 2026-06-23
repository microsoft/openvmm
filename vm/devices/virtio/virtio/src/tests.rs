// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: test code implements a custom `GuestMemory` backing, which requires
// unsafe.
#![expect(unsafe_code)]
#![cfg(test)]

use crate::DeviceTraits;
use crate::DynVirtioDevice;
use crate::PciInterruptModel;
use crate::QueueResources;
use crate::VirtioDevice;
use crate::VirtioQueue;
use crate::VirtioQueueCallbackWork;
use crate::queue::QueueParams;
use crate::queue::QueueState;
use crate::spec::pci::*;
use crate::spec::queue::*;
use crate::spec::*;
use crate::transport::VirtioMmioDevice;
use crate::transport::VirtioPciDevice;
use chipset_device::mmio::ExternallyManagedMmioIntercepts;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pci::PciConfigSpace;
use chipset_device::poll_device::PollDevice;
use futures::StreamExt;
use guestmem::DoorbellRegistration;
use guestmem::GuestMemory;
use guestmem::GuestMemoryAccess;
use guestmem::GuestMemoryBackingError;
use inspect::InspectMut;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_async::timer::PolledTimer;
use pal_async::wait::PolledWait;
use pal_event::Event;
use parking_lot::Mutex;
use pci_core::bus_range::AssignedBusRange;
use pci_core::msi::MsiConnection;
use pci_core::spec::caps::CapabilityId;
use pci_core::spec::cfg_space;
use pci_core::test_helpers::TestPciInterruptController;
use std::collections::BTreeMap;
use std::future::poll_fn;
use std::io;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use task_control::AsyncRun;
use task_control::Cancelled;
use task_control::StopTask;
use task_control::TaskControl;
use test_with_tracing::test;
use vmcore::device_state::ChangeDeviceState;
use vmcore::interrupt::Interrupt;
use vmcore::line_interrupt::LineInterrupt;
use vmcore::line_interrupt::test_helpers::TestLineInterruptTarget;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;

// Device features - first bank
const VIRTIO_F_RING_INDIRECT_DESC: u32 = 0x10000000;
const VIRTIO_F_RING_EVENT_IDX: u32 = 0x20000000;
// Device features - second bank
const VIRTIO_F_VERSION_1: u32 = 1;
const VIRTIO_F_ACCESS_PLATFORM: u32 = 2;
const VIRTIO_F_RING_PACKED: u32 = 4;

// Device status
const VIRTIO_ACKNOWLEDGE: u32 = 1;
const VIRTIO_DRIVER: u32 = 2;
const VIRTIO_DRIVER_OK: u32 = 4;
const VIRTIO_FEATURES_OK: u32 = 8;
const _VIRTIO_DEVICE_NEEDS_RESET: u32 = 0x40;
const _VIRTIO_FAILED: u32 = 0x80;

async fn must_recv_in_timeout<T: 'static + Send>(
    recv: &mut mesh::Receiver<T>,
    timeout: Duration,
) -> T {
    mesh::CancelContext::new()
        .with_timeout(timeout)
        .until_cancelled(recv.next())
        .await
        .unwrap()
        .unwrap()
}

async fn assert_no_recv_in_timeout<T: 'static + Send>(
    recv: &mut mesh::Receiver<T>,
    timeout: Duration,
) {
    if mesh::CancelContext::new()
        .with_timeout(timeout)
        .until_cancelled(recv.next())
        .await
        .is_ok()
    {
        panic!("Expected timeout, but received a value");
    }
}

/// Yield execution to the async executor, allowing spawned tasks to run.
async fn yield_now() {
    let mut yielded = false;
    poll_fn(|cx| {
        if !yielded {
            cx.waker().wake_by_ref();
            yielded = true;
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await
}

/// Yield to the executor then poll the device, allowing a spawned device
/// task to process commands and the transport to observe the result.
async fn yield_and_poll_device(dev: &mut impl PollDevice) {
    yield_now().await;
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    dev.poll_device(&mut cx);
}

#[derive(Default)]
struct VirtioTestMemoryAccess {
    memory_map: Mutex<MemoryMap>,
    doorbell_count: AtomicUsize,
}

#[derive(Default)]
struct MemoryMap {
    map: BTreeMap<u64, (bool, Vec<u8>)>,
}

impl MemoryMap {
    fn get(&mut self, address: u64, len: usize) -> Option<(bool, &mut [u8])> {
        let (&base, &mut (writable, ref mut data)) = self.map.range_mut(..=address).last()?;
        let data = data
            .get_mut(usize::try_from(address - base).ok()?..)?
            .get_mut(..len)?;

        Some((writable, data))
    }

    fn insert(&mut self, address: u64, data: &[u8], writable: bool) {
        if let Some((is_writable, v)) = self.get(address, data.len()) {
            assert_eq!(writable, is_writable);
            v.copy_from_slice(data);
            return;
        }

        let end = address + data.len() as u64;
        let mut data = data.to_vec();
        if let Some((&next, &(next_writable, ref next_data))) = self.map.range(address..).next() {
            if end > next {
                let next_end = next + next_data.len() as u64;
                panic!(
                    "overlapping memory map: {address:#x}..{end:#x} > {next:#x}..={next_end:#x}"
                );
            }
            if end == next && next_writable == writable {
                data.extend(next_data.as_slice());
                self.map.remove(&next).unwrap();
            }
        }

        if let Some((&prev, &mut (prev_writable, ref mut prev_data))) =
            self.map.range_mut(..address).last()
        {
            let prev_end = prev + prev_data.len() as u64;
            if prev_end > address {
                panic!(
                    "overlapping memory map: {prev:#x}..{prev_end:#x} > {address:#x}..={end:#x}"
                );
            }
            if prev_end == address && prev_writable == writable {
                prev_data.extend_from_slice(&data);
                return;
            }
        }

        self.map.insert(address, (writable, data));
    }
}

impl VirtioTestMemoryAccess {
    fn new() -> Arc<Self> {
        Default::default()
    }

    fn modify_memory_map(&self, address: u64, data: &[u8], writeable: bool) {
        self.memory_map.lock().insert(address, data, writeable);
    }

    fn memory_map_get_u16(&self, address: u64) -> u16 {
        let mut map = self.memory_map.lock();
        let (_, data) = map.get(address, 2).unwrap();
        u16::from_le_bytes(data.try_into().unwrap())
    }

    fn memory_map_get_u32(&self, address: u64) -> u32 {
        let mut map = self.memory_map.lock();
        let (_, data) = map.get(address, 4).unwrap();
        u32::from_le_bytes(data.try_into().unwrap())
    }
}

// SAFETY: test code
unsafe impl GuestMemoryAccess for VirtioTestMemoryAccess {
    fn mapping(&self) -> Option<NonNull<u8>> {
        None
    }

    fn max_address(&self) -> u64 {
        // No real bound, so use the max physical address width on
        // AMD64/ARM64.
        1 << 52
    }

    unsafe fn read_fallback(
        &self,
        address: u64,
        dest: *mut u8,
        len: usize,
    ) -> Result<(), GuestMemoryBackingError> {
        match self.memory_map.lock().get(address, len) {
            Some((_, value)) => {
                // SAFETY: guaranteed by caller
                unsafe {
                    std::ptr::copy(value.as_ptr(), dest, len);
                }
            }
            None => panic!(
                "Unexpected read request for {} bytes at address {:x}",
                len, address
            ),
        }
        Ok(())
    }

    unsafe fn write_fallback(
        &self,
        address: u64,
        src: *const u8,
        len: usize,
    ) -> Result<(), GuestMemoryBackingError> {
        match self.memory_map.lock().get(address, len) {
            Some((true, value)) => {
                // SAFETY: guaranteed by caller
                unsafe {
                    std::ptr::copy(src, value.as_mut_ptr(), len);
                }
            }
            _ => panic!(
                "Unexpected write request for {} bytes at address {:x}",
                len, address
            ),
        }
        Ok(())
    }

    fn fill_fallback(
        &self,
        address: u64,
        val: u8,
        len: usize,
    ) -> Result<(), GuestMemoryBackingError> {
        match self.memory_map.lock().get(address, len) {
            Some((true, value)) => value.fill(val),
            _ => panic!("Unexpected write request at address {:x}", address),
        };
        Ok(())
    }
}

struct DoorbellEntry;

impl DoorbellRegistration for VirtioTestMemoryAccess {
    fn register_doorbell(
        &self,
        _: u64,
        _: Option<u64>,
        _: Option<u32>,
        _: &Event,
    ) -> io::Result<Box<dyn Send + Sync>> {
        self.doorbell_count.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(DoorbellEntry))
    }
}

type VirtioTestWorkCallback = Box<dyn Fn(&mut VirtioQueue, VirtioQueueCallbackWork) + Sync + Send>;
struct CreateDirectQueueParams {
    process_work: VirtioTestWorkCallback,
    notify: Interrupt,
    event: Event,
}

struct SplitQueue {
    last_avail_index: Vec<u16>,
    last_used_index: Vec<u16>,
}

struct PackedQueue {
    next_ready_index: Vec<u16>,
    ready_wrapped_bit: Vec<bool>,
    next_completed_index: Vec<u16>,
    completed_wrapped_bit: Vec<bool>,
    buffer_and_count: Vec<BTreeMap<u16, u16>>,
}

enum VirtioQueueInfo {
    Split(SplitQueue),
    Packed(PackedQueue),
}

struct VirtioTestGuest {
    test_mem: Arc<VirtioTestMemoryAccess>,
    driver: DefaultDriver,
    num_queues: u16,
    queue_size: u16,
    allow_indirect_descriptors: bool,
    info: VirtioQueueInfo,
    use_ring_event_index: bool,
    avail_descriptors: Vec<Vec<bool>>,
    auto_arm_event: bool,
}

impl VirtioTestGuest {
    fn new_split(
        driver: &DefaultDriver,
        test_mem: &Arc<VirtioTestMemoryAccess>,
        num_queues: u16,
        queue_size: u16,
        use_ring_event_index: bool,
    ) -> Self {
        let last_avail_index: Vec<u16> = vec![0; num_queues as usize];
        let last_used_index: Vec<u16> = vec![0; num_queues as usize];
        let avail_descriptors: Vec<Vec<bool>> =
            vec![vec![true; queue_size as usize]; num_queues as usize];
        let test_guest = Self {
            test_mem: test_mem.clone(),
            driver: driver.clone(),
            num_queues,
            queue_size,
            allow_indirect_descriptors: true,
            info: VirtioQueueInfo::Split(SplitQueue {
                last_avail_index,
                last_used_index,
            }),
            use_ring_event_index,
            avail_descriptors,
            auto_arm_event: true,
        };
        for i in 0..num_queues {
            test_guest.add_queue_memory(i);
        }
        test_guest
    }

    fn new_packed(
        driver: &DefaultDriver,
        test_mem: &Arc<VirtioTestMemoryAccess>,
        num_queues: u16,
        queue_size: u16,
        use_ring_event_index: bool,
    ) -> Self {
        let avail_descriptors: Vec<Vec<bool>> =
            vec![vec![true; queue_size as usize]; num_queues as usize];
        let test_guest = Self {
            test_mem: test_mem.clone(),
            driver: driver.clone(),
            num_queues,
            queue_size,
            allow_indirect_descriptors: true,
            info: VirtioQueueInfo::Packed(PackedQueue {
                next_ready_index: vec![0; num_queues as usize],
                ready_wrapped_bit: vec![true; num_queues as usize],
                next_completed_index: vec![0; num_queues as usize],
                completed_wrapped_bit: vec![true; num_queues as usize],
                buffer_and_count: vec![BTreeMap::new(); num_queues as usize],
            }),
            use_ring_event_index,
            avail_descriptors,
            auto_arm_event: true,
        };
        for i in 0..num_queues {
            test_guest.add_queue_memory(i);
        }
        test_guest
    }

    fn driver(&self) -> DefaultDriver {
        self.driver.clone()
    }

    fn mem(&self) -> GuestMemory {
        GuestMemory::new("test", self.test_mem.clone())
    }

    fn create_direct_queues<F>(&self, f: F) -> Vec<TaskControl<TestQueueWorker, TestQueueState>>
    where
        F: Fn(u16) -> CreateDirectQueueParams,
    {
        (0..self.num_queues)
            .map(|i| {
                let params = f(i);
                let queue_event = PolledWait::new(&self.driver, params.event).unwrap();
                let queue = VirtioQueue::new(
                    self.queue_features(),
                    self.queue_params(i),
                    self.mem(),
                    params.notify,
                    queue_event,
                    None,
                )
                .expect("failed to create virtio queue");
                let mut tc = TaskControl::new(TestQueueWorker {
                    callback: params.process_work,
                });
                tc.insert(
                    self.driver.clone(),
                    "virtio-test-queue",
                    TestQueueState { queue },
                );
                tc.start();
                tc
            })
            .collect::<Vec<_>>()
    }

    fn queue_features(&self) -> VirtioDeviceFeatures {
        let flags0 = if self.use_ring_event_index {
            VIRTIO_F_RING_EVENT_IDX
        } else {
            0
        };
        let flags0 = if self.allow_indirect_descriptors {
            flags0 | VIRTIO_F_RING_INDIRECT_DESC
        } else {
            flags0
        };

        let flags1 = VIRTIO_F_VERSION_1;
        let flags1 = if matches!(self.info, VirtioQueueInfo::Packed(_)) {
            flags1 | VIRTIO_F_RING_PACKED
        } else {
            flags1
        };

        VirtioDeviceFeatures::new()
            .with_bank(0, flags0)
            .with_bank(1, flags1)
    }

    fn queue_params(&self, i: u16) -> QueueParams {
        QueueParams {
            size: self.queue_size,
            enable: true,
            desc_addr: self.get_queue_descriptor_base_address(i),
            avail_addr: self.get_queue_available_base_address(i),
            used_addr: self.get_queue_used_base_address(i),
        }
    }

    fn get_queue_base_address(&self, index: u16) -> u64 {
        0x10000000 * index as u64
    }

    fn get_queue_descriptor_base_address(&self, index: u16) -> u64 {
        self.get_queue_base_address(index) + 0x1000
    }

    fn get_queue_available_base_address(&self, index: u16) -> u64 {
        self.get_queue_base_address(index) + 0x2000
    }

    fn get_queue_used_base_address(&self, index: u16) -> u64 {
        self.get_queue_base_address(index) + 0x3000
    }

    fn get_queue_descriptor_backing_memory_address(&self, index: u16) -> u64 {
        self.get_queue_base_address(index) + 0x4000
    }

    async fn setup_chipset_device(
        &self,
        dev: &mut VirtioMmioDevice,
        driver_features: VirtioDeviceFeatures,
    ) {
        dev.write_u32(112, VIRTIO_ACKNOWLEDGE);
        dev.write_u32(112, VIRTIO_DRIVER);
        dev.write_u32(36, 0);
        dev.write_u32(32, driver_features.bank(0));
        dev.write_u32(36, 1);
        dev.write_u32(32, driver_features.bank(1));
        dev.write_u32(112, VIRTIO_FEATURES_OK);
        for i in 0..self.num_queues {
            let queue_index = i;
            dev.write_u32(48, i as u32);
            dev.write_u32(56, self.queue_size as u32);
            let desc_addr = self.get_queue_descriptor_base_address(queue_index);
            dev.write_u32(128, desc_addr as u32);
            dev.write_u32(132, (desc_addr >> 32) as u32);
            let avail_addr = self.get_queue_available_base_address(queue_index);
            dev.write_u32(144, avail_addr as u32);
            dev.write_u32(148, (avail_addr >> 32) as u32);
            let used_addr = self.get_queue_used_base_address(queue_index);
            dev.write_u32(160, used_addr as u32);
            dev.write_u32(164, (used_addr >> 32) as u32);
            // enable the queue
            dev.write_u32(68, 1);
        }
        dev.write_u32(112, VIRTIO_DRIVER_OK);
        yield_and_poll_device(dev).await;
        assert_eq!(dev.read_u32(0xfc), 2);
    }

    async fn setup_pci_device(
        &self,
        dev: &mut VirtioPciTestDevice,
        driver_features: VirtioDeviceFeatures,
    ) {
        let bar_address1: u64 = 0x10000000000;
        dev.pci_device
            .pci_cfg_write(0x14, (bar_address1 >> 32) as u32)
            .unwrap();
        dev.pci_device
            .pci_cfg_write(0x10, bar_address1 as u32)
            .unwrap();

        let bar_address2: u64 = 0x20000000000;
        dev.pci_device
            .pci_cfg_write(0x1c, (bar_address2 >> 32) as u32)
            .unwrap();
        dev.pci_device
            .pci_cfg_write(0x18, bar_address2 as u32)
            .unwrap();

        dev.pci_device
            .pci_cfg_write(
                0x4,
                cfg_space::Command::new()
                    .with_mmio_enabled(true)
                    .into_bits() as u32,
            )
            .unwrap();

        let mut device_status = VIRTIO_ACKNOWLEDGE as u8;
        dev.pci_device
            .mmio_write(bar_address1 + 20, &device_status.to_le_bytes())
            .unwrap();
        device_status = VIRTIO_DRIVER as u8;
        dev.pci_device
            .mmio_write(bar_address1 + 20, &device_status.to_le_bytes())
            .unwrap();
        dev.write_u32(bar_address1 + 8, 0);
        dev.write_u32(bar_address1 + 12, driver_features.bank(0));
        dev.write_u32(bar_address1 + 8, 1);
        dev.write_u32(bar_address1 + 12, driver_features.bank(1));
        device_status = VIRTIO_FEATURES_OK as u8;
        dev.pci_device
            .mmio_write(bar_address1 + 20, &device_status.to_le_bytes())
            .unwrap();
        // setup config interrupt
        dev.pci_device
            .mmio_write(bar_address2, &0_u64.to_le_bytes())
            .unwrap(); // vector
        dev.pci_device
            .mmio_write(bar_address2 + 8, &0_u32.to_le_bytes())
            .unwrap(); // data
        dev.pci_device
            .mmio_write(bar_address2 + 12, &0_u32.to_le_bytes())
            .unwrap();
        for i in 0..self.num_queues {
            let queue_index = i;
            dev.pci_device
                .mmio_write(bar_address1 + 22, &queue_index.to_le_bytes())
                .unwrap();
            dev.pci_device
                .mmio_write(bar_address1 + 24, &self.queue_size.to_le_bytes())
                .unwrap();
            // setup MSI information for the queue
            let msix_vector = queue_index + 1;
            let address = bar_address2 + 0x10 * msix_vector as u64;
            dev.pci_device
                .mmio_write(address, &(msix_vector as u64).to_le_bytes())
                .unwrap();
            let address = bar_address2 + 0x10 * msix_vector as u64 + 8;
            dev.pci_device
                .mmio_write(address, &0_u32.to_le_bytes())
                .unwrap();
            let address = bar_address2 + 0x10 * msix_vector as u64 + 12;
            dev.pci_device
                .mmio_write(address, &0_u32.to_le_bytes())
                .unwrap();
            dev.pci_device
                .mmio_write(bar_address1 + 26, &msix_vector.to_le_bytes())
                .unwrap();
            // setup queue addresses
            let desc_addr = self.get_queue_descriptor_base_address(queue_index);
            dev.write_u32(bar_address1 + 32, desc_addr as u32);
            dev.write_u32(bar_address1 + 36, (desc_addr >> 32) as u32);
            let avail_addr = self.get_queue_available_base_address(queue_index);
            dev.write_u32(bar_address1 + 40, avail_addr as u32);
            dev.write_u32(bar_address1 + 44, (avail_addr >> 32) as u32);
            let used_addr = self.get_queue_used_base_address(queue_index);
            dev.write_u32(bar_address1 + 48, used_addr as u32);
            dev.write_u32(bar_address1 + 52, (used_addr >> 32) as u32);
            // enable the queue
            let enabled: u16 = 1;
            dev.pci_device
                .mmio_write(bar_address1 + 28, &enabled.to_le_bytes())
                .unwrap();
        }
        // enable all device MSI interrupts
        dev.pci_device.pci_cfg_write(0x40, 0x80000000).unwrap();
        // run device — use the write_u32 test helper to bypass MmioIntercept
        // stall/deferred logic.
        let current = dev.pci_device.read_u32(20);
        dev.pci_device
            .write_u32(20, (current & !0xff) | VIRTIO_DRIVER_OK);
        yield_and_poll_device(&mut dev.pci_device).await;
        let config_generation = (dev.pci_device.read_u32(20) >> 8) & 0xff;
        assert_eq!(config_generation, 2);
    }

    fn get_queue_descriptor(&self, queue_index: u16, descriptor_index: u16) -> u64 {
        self.get_queue_descriptor_base_address(queue_index) + 0x10 * descriptor_index as u64
    }

    fn add_queue_memory(&self, queue_index: u16) {
        let is_packed_descriptors = matches!(self.info, VirtioQueueInfo::Packed(_));
        // descriptors
        for i in 0..self.queue_size {
            let base = self.get_queue_descriptor(queue_index, i);
            // physical address
            self.test_mem.modify_memory_map(
                base,
                &(self.get_queue_descriptor_backing_memory_address(queue_index)
                    + 0x1000 * i as u64)
                    .to_le_bytes(),
                is_packed_descriptors,
            );
            // length
            self.test_mem.modify_memory_map(
                base + 8,
                &0x1000u32.to_le_bytes(),
                is_packed_descriptors,
            );
            // split: flags, packed: buffer_id
            self.test_mem
                .modify_memory_map(base + 12, &0u16.to_le_bytes(), is_packed_descriptors);
            // split: next index, packed: flags
            self.test_mem
                .modify_memory_map(base + 14, &0u16.to_le_bytes(), is_packed_descriptors);
        }

        if is_packed_descriptors {
            let base = self.get_queue_descriptor(queue_index, self.queue_size);
            // Device and driver event fields
            self.test_mem
                .modify_memory_map(base, &0_u32.to_le_bytes(), true);
            self.test_mem
                .modify_memory_map(base + 4, &0_u32.to_le_bytes(), false);
            self.test_mem.modify_memory_map(
                self.get_queue_available_base_address(queue_index),
                &0_u32.to_le_bytes(),
                false,
            );
            self.test_mem.modify_memory_map(
                self.get_queue_used_base_address(queue_index),
                &0_u32.to_le_bytes(),
                true,
            );
            return;
        }

        // available queue (flags, index)
        let base = self.get_queue_available_base_address(queue_index);
        self.test_mem
            .modify_memory_map(base, &0u16.to_le_bytes(), false);
        self.test_mem
            .modify_memory_map(base + 2, &0u16.to_le_bytes(), false);
        // available queue ring buffer
        for i in 0..self.queue_size {
            let base = base + 4 + 2 * i as u64;
            self.test_mem
                .modify_memory_map(base, &0u16.to_le_bytes(), false);
        }
        // used event
        if self.use_ring_event_index {
            self.test_mem.modify_memory_map(
                base + 4 + 2 * self.queue_size as u64,
                &0u16.to_le_bytes(),
                false,
            );
        }

        // used queue (flags, index)
        let base = self.get_queue_used_base_address(queue_index);
        self.test_mem
            .modify_memory_map(base, &0u16.to_le_bytes(), true);
        self.test_mem
            .modify_memory_map(base + 2, &0u16.to_le_bytes(), true);
        for i in 0..self.queue_size {
            let base = base + 4 + 8 * i as u64;
            // index
            self.test_mem
                .modify_memory_map(base, &0u32.to_le_bytes(), true);
            // length
            self.test_mem
                .modify_memory_map(base + 4, &0u32.to_le_bytes(), true);
        }
        // available event
        if self.use_ring_event_index {
            self.test_mem.modify_memory_map(
                base + 4 + 8 * self.queue_size as u64,
                &0u16.to_le_bytes(),
                true,
            );
        }
    }

    fn reserve_split_descriptor(&mut self, queue_index: u16) -> u16 {
        if let Some((desc_index, desc)) = self.avail_descriptors[queue_index as usize]
            .iter_mut()
            .enumerate()
            .find(|(_, desc)| **desc)
        {
            *desc = false;
            return desc_index as u16;
        }
        panic!("No descriptors are available!");
    }

    fn reserve_packed_descriptors(&mut self, queue_index: u16, count: u16) -> u16 {
        let queue_size = self.queue_size;
        let starting_descriptor = if let VirtioQueueInfo::Packed(packed) = &self.info {
            packed.next_ready_index[queue_index as usize]
        } else {
            panic!("Not a packed queue");
        };
        let avail_descriptors = &mut self.avail_descriptors[queue_index as usize];
        for i in 0..count {
            let desc_index = (starting_descriptor + i) % queue_size;
            if !avail_descriptors[desc_index as usize] {
                panic!("Not enough available descriptors!");
            }
            avail_descriptors[desc_index as usize] = false;
        }
        // Reset default descriptor values.
        for i in 0..count {
            let desc_index = (starting_descriptor + i) % queue_size;
            let base = self.get_queue_descriptor(queue_index, desc_index);
            // physical address
            self.test_mem.modify_memory_map(
                base,
                &(self.get_queue_descriptor_backing_memory_address(queue_index)
                    + 0x1000 * desc_index as u64)
                    .to_le_bytes(),
                true,
            );
            // length
            self.test_mem
                .modify_memory_map(base + 8, &0x1000u32.to_le_bytes(), true);
            // buffer_id
            self.test_mem
                .modify_memory_map(base + 12, &0u16.to_le_bytes(), true);
            // flags
            self.test_mem
                .modify_memory_map(base + 14, &0u16.to_le_bytes(), true);
        }
        starting_descriptor
    }

    fn free_descriptor(&mut self, queue_index: u16, desc_index: u16) {
        assert!(desc_index < self.queue_size);
        let desc_addr = self.get_queue_descriptor(queue_index, desc_index);
        if matches!(self.info, VirtioQueueInfo::Split(_)) {
            let flags: DescriptorFlags = self.test_mem.memory_map_get_u16(desc_addr + 12).into();
            if flags.next() {
                let next = self.test_mem.memory_map_get_u16(desc_addr + 14);
                self.free_descriptor(queue_index, next);
            }
        } else {
            // Linked packed descriptors are handled in get_next_packed_completed()
        }
        let avail_descriptors = &mut self.avail_descriptors[queue_index as usize];
        assert_eq!(avail_descriptors[desc_index as usize], false);
        avail_descriptors[desc_index as usize] = true;
    }

    fn queue_available_desc(&mut self, queue_index: u16, desc_index: u16) {
        let avail_base_addr = self.get_queue_available_base_address(queue_index);
        let (last_avail_index, next_index) = if let VirtioQueueInfo::Split(split) = &mut self.info {
            let last_avail_index = split.last_avail_index[queue_index as usize];
            let next_index = last_avail_index % self.queue_size;
            let last_avail_index = last_avail_index.wrapping_add(1);
            split.last_avail_index[queue_index as usize] = last_avail_index;
            (last_avail_index, next_index)
        } else {
            panic!("Not a split queue");
        };
        self.test_mem.modify_memory_map(
            avail_base_addr + 4 + 2 * next_index as u64,
            &desc_index.to_le_bytes(),
            false,
        );
        self.test_mem.modify_memory_map(
            avail_base_addr + 2,
            &last_avail_index.to_le_bytes(),
            false,
        );
    }

    fn add_to_avail_queue(&mut self, queue_index: u16) {
        if matches!(self.info, VirtioQueueInfo::Packed(_)) {
            self.make_packed_descriptors_available(queue_index, vec![DescriptorFlags::new()], None);
        } else {
            let next_descriptor = self.reserve_split_descriptor(queue_index);
            // flags
            self.test_mem.modify_memory_map(
                self.get_queue_descriptor(queue_index, next_descriptor) + 12,
                &0u16.to_le_bytes(),
                false,
            );
            self.queue_available_desc(queue_index, next_descriptor);
        }
    }

    fn make_packed_descriptors_available(
        &mut self,
        queue_index: u16,
        flags: Vec<DescriptorFlags>,
        indirect_count: Option<u32>,
    ) {
        let count = flags.len() as u16;
        let first_descriptor = self.reserve_packed_descriptors(queue_index, count);
        let wrapped_bit = if let VirtioQueueInfo::Packed(packed) = &self.info {
            packed.ready_wrapped_bit[queue_index as usize]
        } else {
            unreachable!("Not a packed queue");
        };
        let buffer_id = queue_index << 8 | first_descriptor;
        // The first descriptor needs to be the last marked available, as once
        // it is valid, all the descriptors can be examined at any time.
        for desc_index in (0..count).rev() {
            let descriptor_index = (first_descriptor + desc_index) % self.queue_size;
            let wrapped_bit = if descriptor_index >= first_descriptor {
                wrapped_bit
            } else {
                !wrapped_bit
            };
            let desc_addr = self.get_queue_descriptor(queue_index, descriptor_index);
            let flags = flags[desc_index as usize]
                .with_available(wrapped_bit)
                .with_used(!wrapped_bit)
                .with_next(desc_index < count - 1);
            self.test_mem
                .modify_memory_map(desc_addr + 14, &flags.into_bits().to_le_bytes(), true);
            if desc_index == count - 1 {
                // The buffer ID is assigned to the last descriptor.
                self.test_mem
                    .modify_memory_map(desc_addr + 12, &buffer_id.to_le_bytes(), true);
                // Indirect packed descriptors consume all of the indirect
                // buffer, so set the size accordingly.
                if let Some(indirect_count) = indirect_count {
                    let length = indirect_count * 0x10;
                    self.test_mem
                        .modify_memory_map(desc_addr + 8, &length.to_le_bytes(), true);
                }
            }
        }
        if let VirtioQueueInfo::Packed(packed) = &mut self.info {
            packed.buffer_and_count[queue_index as usize].insert(buffer_id, count);
            let next_ready_index = (first_descriptor + count) % self.queue_size;
            if next_ready_index < packed.next_ready_index[queue_index as usize] {
                packed.ready_wrapped_bit[queue_index as usize] =
                    !packed.ready_wrapped_bit[queue_index as usize];
            }
            packed.next_ready_index[queue_index as usize] = next_ready_index;
        } else {
            unreachable!("Not a packed queue");
        };
    }

    fn add_indirect_to_avail_queue(&mut self, queue_index: u16) {
        // create another (indirect) descriptor in the buffer
        let buffer_addr = self.get_queue_descriptor_backing_memory_address(queue_index);
        // physical address
        self.test_mem
            .modify_memory_map(buffer_addr, &0xffffffff00000000u64.to_le_bytes(), false);
        // length
        self.test_mem
            .modify_memory_map(buffer_addr + 8, &0x1000u32.to_le_bytes(), false);
        // split: flags, packed: buffer_id (ignored for indirect descriptors)
        self.test_mem
            .modify_memory_map(buffer_addr + 12, &0u16.to_le_bytes(), false);
        // split: next, packed: flags (wait bit ignored)
        self.test_mem
            .modify_memory_map(buffer_addr + 14, &0u16.to_le_bytes(), false);
        if matches!(self.info, VirtioQueueInfo::Packed(_)) {
            self.make_packed_descriptors_available(
                queue_index,
                vec![DescriptorFlags::new().with_indirect(true)],
                Some(1),
            );
        } else {
            let next_descriptor = self.reserve_split_descriptor(queue_index);
            // flags on primary descriptor
            self.test_mem.modify_memory_map(
                self.get_queue_descriptor(queue_index, next_descriptor) + 12,
                &u16::from(DescriptorFlags::new().with_indirect(true)).to_le_bytes(),
                false,
            );
            self.queue_available_desc(queue_index, next_descriptor);
        }
    }

    fn add_linked_to_avail_queue(&mut self, queue_index: u16, desc_count: u16) {
        if matches!(self.info, VirtioQueueInfo::Packed(_)) {
            let flags = vec![DescriptorFlags::new(); desc_count as usize];
            return self.make_packed_descriptors_available(queue_index, flags, None);
        }

        let mut descriptors = Vec::with_capacity(desc_count as usize);
        for _ in 0..desc_count {
            descriptors.push(self.reserve_split_descriptor(queue_index));
        }

        for i in 0..descriptors.len() {
            let base = self.get_queue_descriptor(queue_index, descriptors[i]);
            let flags = if i < descriptors.len() - 1 {
                u16::from(DescriptorFlags::new().with_next(true))
            } else {
                0
            };
            self.test_mem
                .modify_memory_map(base + 12, &flags.to_le_bytes(), false);
            let next = if i < descriptors.len() - 1 {
                descriptors[i + 1]
            } else {
                0
            };
            self.test_mem
                .modify_memory_map(base + 14, &next.to_le_bytes(), false);
        }
        self.queue_available_desc(queue_index, descriptors[0]);
    }

    fn add_indirect_linked_to_avail_queue(&mut self, queue_index: u16, desc_count: u16) {
        // create indirect descriptors in the buffer
        let buffer_addr = self.get_queue_descriptor_backing_memory_address(queue_index);
        for i in 0..desc_count {
            let base = buffer_addr + 0x10 * i as u64;
            let indirect_buffer_addr = 0xffffffff00000000u64 + 0x1000 * i as u64;
            // physical address
            self.test_mem
                .modify_memory_map(base, &indirect_buffer_addr.to_le_bytes(), false);
            // length
            self.test_mem
                .modify_memory_map(base + 8, &0x1000u32.to_le_bytes(), false);
            // The next field is ignored for packed indirect descriptors. It
            // will consume all descriptors in the buffer, so is dependent on
            // the size of the buffer.
            let flags = if matches!(self.info, VirtioQueueInfo::Split(_)) && i < desc_count - 1 {
                DescriptorFlags::new().with_next(true).into_bits()
            } else {
                0
            };
            if matches!(self.info, VirtioQueueInfo::Packed(_)) {
                // buffer id (ignored for indirect descriptors)
                self.test_mem
                    .modify_memory_map(base + 12, &0_u16.to_le_bytes(), false);
                // flags
                self.test_mem
                    .modify_memory_map(base + 14, &flags.to_le_bytes(), false);
            } else {
                // flags
                self.test_mem
                    .modify_memory_map(base + 12, &flags.to_le_bytes(), false);
                // next index
                let next = if i < desc_count - 1 { i + 1 } else { 0 };
                self.test_mem
                    .modify_memory_map(base + 14, &next.to_le_bytes(), false);
            }
        }

        if matches!(self.info, VirtioQueueInfo::Packed(_)) {
            self.make_packed_descriptors_available(
                queue_index,
                vec![DescriptorFlags::new().with_indirect(true)],
                Some(desc_count as u32),
            );
        } else {
            let next_descriptor = self.reserve_split_descriptor(queue_index);
            // flags on primary descriptor
            self.test_mem.modify_memory_map(
                self.get_queue_descriptor(queue_index, next_descriptor) + 12,
                &DescriptorFlags::new()
                    .with_indirect(true)
                    .into_bits()
                    .to_le_bytes(),
                false,
            );
            self.queue_available_desc(queue_index, next_descriptor);
        }
    }

    fn get_next_completed(&mut self, queue_index: u16) -> Option<(u16, u32)> {
        if matches!(self.info, VirtioQueueInfo::Split(_)) {
            self.get_next_split_completed(queue_index)
        } else {
            self.get_next_packed_completed(queue_index)
        }
    }

    fn get_next_split_completed(&mut self, queue_index: u16) -> Option<(u16, u32)> {
        let avail_base_addr = self.get_queue_available_base_address(queue_index);
        let used_base_addr = self.get_queue_used_base_address(queue_index);
        let cur_used_index = self.test_mem.memory_map_get_u16(used_base_addr + 2);
        let next_index = if let VirtioQueueInfo::Split(split) = &mut self.info {
            let last_used_index = split.last_used_index[queue_index as usize];
            if last_used_index == cur_used_index {
                return None;
            }
            split.last_used_index[queue_index as usize] = last_used_index.wrapping_add(1);
            last_used_index % self.queue_size
        } else {
            panic!("Not a split queue");
        };

        if self.auto_arm_event && self.use_ring_event_index {
            self.test_mem.modify_memory_map(
                avail_base_addr + 4 + 2 * self.queue_size as u64,
                &cur_used_index.to_le_bytes(),
                false,
            );
        }

        let desc_index = self
            .test_mem
            .memory_map_get_u32(used_base_addr + 4 + 8 * next_index as u64);
        let desc_index = desc_index as u16;
        let bytes_written = self
            .test_mem
            .memory_map_get_u32(used_base_addr + 8 + 8 * next_index as u64);
        self.free_descriptor(queue_index, desc_index);
        Some((desc_index, bytes_written))
    }

    fn get_next_packed_completed(&mut self, queue_index: u16) -> Option<(u16, u32)> {
        let queue_size = self.queue_size;
        let (descriptor_index, wrapped_bit) =
            if let VirtioQueueInfo::Packed(packed) = &mut self.info {
                (
                    packed.next_completed_index[queue_index as usize],
                    packed.completed_wrapped_bit[queue_index as usize],
                )
            } else {
                panic!("Not a packed queue");
            };
        let desc_addr = self.get_queue_descriptor(queue_index, descriptor_index);
        let flags: DescriptorFlags = self.test_mem.memory_map_get_u16(desc_addr + 14).into();
        if flags.available() != wrapped_bit || flags.used() != wrapped_bit {
            return None;
        }

        let bytes_written = self.test_mem.memory_map_get_u32(desc_addr + 8);
        let buffer_id = self.test_mem.memory_map_get_u16(desc_addr + 12);
        let count = if let VirtioQueueInfo::Packed(packed) = &mut self.info {
            let err_msg = format!("Buffer ID {} not found in queue {}", buffer_id, queue_index);
            let count = packed
                .buffer_and_count
                .get_mut(queue_index as usize)
                .expect("Invalid queue index")
                .remove(&buffer_id)
                .expect(err_msg.as_str());
            let next_completed_index = (descriptor_index + count) % queue_size;
            if next_completed_index < packed.next_completed_index[queue_index as usize] {
                packed.completed_wrapped_bit[queue_index as usize] =
                    !packed.completed_wrapped_bit[queue_index as usize];
            }
            packed.next_completed_index[queue_index as usize] = next_completed_index;
            count
        } else {
            unreachable!("Not a packed queue");
        };

        for desc_index in 0..count {
            let descriptor_index = (descriptor_index + desc_index) % queue_size;
            self.free_descriptor(queue_index, descriptor_index);
        }
        Some((descriptor_index, bytes_written))
    }

    fn enable_interrupt(&mut self, queue_index: u16, desc_index: Option<u16>) {
        assert!(desc_index.is_none() || self.use_ring_event_index);
        let base = self.get_queue_available_base_address(queue_index);
        if let VirtioQueueInfo::Packed(packed) = &mut self.info {
            if let Some(desc_index) = desc_index {
                let wrapped_bit = if packed.next_ready_index[queue_index as usize] > desc_index {
                    if packed.ready_wrapped_bit[queue_index as usize] {
                        0
                    } else {
                        1
                    }
                } else {
                    if packed.ready_wrapped_bit[queue_index as usize] {
                        1
                    } else {
                        0
                    }
                };
                let packed_event = desc_index as u32 | (wrapped_bit << 15) | (2_u32 << 16);
                self.test_mem
                    .modify_memory_map(base, &packed_event.to_le_bytes(), false);
                self.auto_arm_event = false;
            } else {
                self.test_mem
                    .modify_memory_map(base, &0_u32.to_le_bytes(), false);
                self.auto_arm_event = true;
            }
        } else {
            if let Some(desc_index) = desc_index {
                self.test_mem.modify_memory_map(
                    base + 4 + 2 * self.queue_size as u64,
                    &desc_index.to_le_bytes(),
                    false,
                );
                self.auto_arm_event = false;
            } else {
                if self.use_ring_event_index {
                    let next_index = if let VirtioQueueInfo::Split(split) = &self.info {
                        split.last_used_index[queue_index as usize]
                    } else {
                        panic!("Not a split queue");
                    };
                    self.test_mem.modify_memory_map(
                        base + 4 + 2 * self.queue_size as u64,
                        &next_index.to_le_bytes(),
                        false,
                    );
                } else {
                    self.test_mem
                        .modify_memory_map(base, &0_u16.to_le_bytes(), false);
                }
                self.auto_arm_event = true;
            }
        }
    }

    fn disable_interrupt(&mut self, queue_index: u16) {
        let base = self.get_queue_available_base_address(queue_index);
        if matches!(self.info, VirtioQueueInfo::Packed(_)) {
            self.test_mem
                .modify_memory_map(base, &(1_u32 << 16).to_le_bytes(), false);
        } else {
            if self.use_ring_event_index {
                // Can't really disable, but can set the next event to be far away.
                let last_index = if let VirtioQueueInfo::Split(split) = &self.info {
                    split.last_used_index[queue_index as usize] - 1
                } else {
                    panic!("Not a split queue");
                };
                self.test_mem.modify_memory_map(
                    base + 4 + 2 * self.queue_size as u64,
                    &last_index.to_le_bytes(),
                    false,
                );
            } else {
                self.test_mem
                    .modify_memory_map(base, &1_u16.to_le_bytes(), false);
            }
        }
        self.auto_arm_event = false;
    }
}

struct TestQueueWorker {
    callback: VirtioTestWorkCallback,
}

struct TestQueueState {
    queue: VirtioQueue,
}

impl AsyncRun<TestQueueState> for TestQueueWorker {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        state: &mut TestQueueState,
    ) -> Result<(), Cancelled> {
        loop {
            let work = stop.until_stopped(state.queue.next()).await?;
            let Some(work) = work else { break };
            match work {
                Ok(work) => (self.callback)(&mut state.queue, work),
                Err(err) => panic!("queue error: {}", err),
            }
        }
        Ok(())
    }
}
struct VirtioPciTestDevice {
    pci_device: VirtioPciDevice,
    test_intc: Arc<TestPciInterruptController>,
}

type TestDeviceQueueWorkFn =
    Arc<dyn Fn(u16, &mut VirtioQueue, VirtioQueueCallbackWork) + Send + Sync>;

/// A minimal VirtioDevice whose start_queue() always returns an error.
/// Used to test that transports correctly handle enable failures.
#[derive(InspectMut)]
#[inspect(skip)]
struct FailingTestDevice {
    traits: DeviceTraits,
}

impl VirtioDevice for FailingTestDevice {
    fn traits(&self) -> DeviceTraits {
        self.traits.clone()
    }

    async fn read_registers_u32(&mut self, _offset: u16) -> u32 {
        0
    }

    async fn write_registers_u32(&mut self, _offset: u16, _val: u32) {}

    async fn start_queue(
        &mut self,
        _idx: u16,
        _resources: QueueResources,
        _features: &VirtioDeviceFeatures,
        _initial_state: Option<QueueState>,
    ) -> anyhow::Result<()> {
        anyhow::bail!("intentional enable failure for testing")
    }

    async fn stop_queue(&mut self, _idx: u16) -> Option<QueueState> {
        None
    }
}

#[derive(InspectMut)]
#[inspect(skip)]
struct TestDevice {
    traits: DeviceTraits,
    queue_work: Option<TestDeviceQueueWorkFn>,
    driver: vmcore::vm_task::VmTaskDriver,
    workers: Vec<TaskControl<TestDeviceTask, TestDeviceQueue>>,
}

impl TestDevice {
    fn new(
        driver_source: &VmTaskDriverSource,
        traits: DeviceTraits,
        queue_work: Option<TestDeviceQueueWorkFn>,
    ) -> Self {
        Self {
            traits,
            queue_work,
            driver: driver_source.simple(),
            workers: Vec::new(),
        }
    }
}

impl VirtioDevice for TestDevice {
    fn traits(&self) -> DeviceTraits {
        self.traits.clone()
    }

    async fn read_registers_u32(&mut self, _offset: u16) -> u32 {
        0
    }

    async fn write_registers_u32(&mut self, _offset: u16, _val: u32) {}

    async fn start_queue(
        &mut self,
        idx: u16,
        resources: QueueResources,
        features: &VirtioDeviceFeatures,
        initial_state: Option<QueueState>,
    ) -> anyhow::Result<()> {
        let mut tc = TaskControl::new(TestDeviceTask {
            index: idx,
            queue_work: self.queue_work.clone(),
        });

        let queue_event = PolledWait::new(&self.driver, resources.event).unwrap();
        let queue = VirtioQueue::new(
            *features,
            resources.params,
            resources.guest_memory,
            resources.notify,
            queue_event,
            initial_state,
        )
        .expect("failed to create virtio queue");

        tc.insert(
            self.driver.clone(),
            "virtio-test-queue",
            TestDeviceQueue { queue },
        );
        tc.start();

        let idx = idx as usize;
        if idx >= self.workers.len() {
            self.workers.resize_with(idx + 1, || {
                TaskControl::new(TestDeviceTask {
                    index: 0,
                    queue_work: None,
                })
            });
        }
        self.workers[idx] = tc;
        Ok(())
    }

    async fn stop_queue(&mut self, idx: u16) -> Option<QueueState> {
        let idx = idx as usize;
        if idx >= self.workers.len() || !self.workers[idx].has_state() {
            return None;
        }
        self.workers[idx].stop().await;
        let state = self.workers[idx].remove().queue.queue_state();
        Some(state)
    }

    async fn reset(&mut self) {
        self.workers.clear();
    }

    fn supports_save_restore(&self) -> bool {
        true
    }
}

struct TestDeviceTask {
    index: u16,
    queue_work: Option<TestDeviceQueueWorkFn>,
}

struct TestDeviceQueue {
    queue: VirtioQueue,
}

impl AsyncRun<TestDeviceQueue> for TestDeviceTask {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        state: &mut TestDeviceQueue,
    ) -> Result<(), Cancelled> {
        loop {
            let work = stop.until_stopped(state.queue.next()).await?;
            let Some(work) = work else { break };
            match work {
                Ok(work) => {
                    if let Some(ref func) = self.queue_work {
                        (func)(self.index, &mut state.queue, work);
                    }
                }
                Err(err) => {
                    panic!(
                        "Invalid virtio queue state index {} error {}",
                        self.index, err
                    );
                }
            }
        }
        Ok(())
    }
}

impl VirtioPciTestDevice {
    fn new(
        driver: &DefaultDriver,
        num_queues: u16,
        test_mem: &Arc<VirtioTestMemoryAccess>,
        queue_work: Option<TestDeviceQueueWorkFn>,
    ) -> Self {
        let doorbell_registration: Arc<dyn DoorbellRegistration> = test_mem.clone();
        let mem = GuestMemory::new("test", test_mem.clone());
        let msi_conn = MsiConnection::new(AssignedBusRange::new(), 0);
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));

        let dev = VirtioPciDevice::new(
            Box::new(TestDevice::new(
                &driver_source,
                DeviceTraits {
                    device_id: VirtioDeviceType::CONSOLE,
                    device_features: VirtioDeviceFeatures::new()
                        .with_bank(0, 2 | VIRTIO_F_RING_INDIRECT_DESC | VIRTIO_F_RING_EVENT_IDX)
                        .with_bank(1, VIRTIO_F_RING_PACKED),
                    max_queues: num_queues,
                    device_register_length: 12,
                    ..Default::default()
                },
                queue_work,
            )),
            driver,
            mem.clone(),
            PciInterruptModel::Msix(msi_conn.target()),
            Some(doorbell_registration),
            &mut ExternallyManagedMmioIntercepts,
            None,
        )
        .unwrap();

        let test_intc = Arc::new(TestPciInterruptController::new());
        msi_conn.connect(test_intc.signal_msi());

        Self {
            pci_device: dev,
            test_intc,
        }
    }

    fn read_u32(&mut self, address: u64) -> u32 {
        let mut value = [0; 4];
        self.pci_device.mmio_read(address, &mut value).unwrap();
        u32::from_ne_bytes(value)
    }

    fn write_u32(&mut self, address: u64, value: u32) {
        self.pci_device
            .mmio_write(address, &value.to_ne_bytes())
            .unwrap();
    }
}

#[async_test]
async fn verify_chipset_config(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    let doorbell_registration: Arc<dyn DoorbellRegistration> = test_mem.clone();
    let mem = GuestMemory::new("test", test_mem);
    let interrupt = LineInterrupt::detached();
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));

    let mut dev = VirtioMmioDevice::new(
        Box::new(TestDevice::new(
            &driver_source,
            DeviceTraits {
                device_id: VirtioDeviceType::CONSOLE,
                device_features: VirtioDeviceFeatures::new()
                    .with_bank(0, 2 | VIRTIO_F_RING_INDIRECT_DESC | VIRTIO_F_RING_EVENT_IDX)
                    .with_bank(1, VIRTIO_F_RING_PACKED),
                max_queues: 1,
                device_register_length: 0,
                ..Default::default()
            },
            None,
        )),
        &driver_source.simple(),
        mem.clone(),
        interrupt,
        Some(doorbell_registration),
        0,
        1,
    )
    .unwrap();
    // magic value
    assert_eq!(dev.read_u32(0), u32::from_le_bytes(*b"virt"));
    // version
    assert_eq!(dev.read_u32(4), 2);
    // device ID
    assert_eq!(dev.read_u32(8), 3);
    // vendor ID
    assert_eq!(dev.read_u32(12), 0x1af4);
    // device feature (bank 0)
    assert_eq!(
        dev.read_u32(16),
        VIRTIO_F_RING_INDIRECT_DESC | VIRTIO_F_RING_EVENT_IDX | 2
    );
    // device feature bank index
    assert_eq!(dev.read_u32(20), 0);
    // device feature (bank 1)
    dev.write_u32(20, 1);
    assert_eq!(dev.read_u32(20), 1);
    assert_eq!(
        dev.read_u32(16),
        VIRTIO_F_VERSION_1 | VIRTIO_F_ACCESS_PLATFORM | VIRTIO_F_RING_PACKED
    );
    // device feature (bank 2)
    dev.write_u32(20, 2);
    assert_eq!(dev.read_u32(16), 0);
    // driver feature (bank 0)
    assert_eq!(dev.read_u32(32), 0);
    dev.write_u32(32, 2);
    assert_eq!(dev.read_u32(32), 2);
    dev.write_u32(32, 0xffffffff);
    assert_eq!(
        dev.read_u32(32),
        VIRTIO_F_RING_INDIRECT_DESC | VIRTIO_F_RING_EVENT_IDX | 2
    );
    // driver feature bank index
    assert_eq!(dev.read_u32(36), 0);
    dev.write_u32(36, 1);
    assert_eq!(dev.read_u32(36), 1);
    // driver feature (bank 1)
    assert_eq!(dev.read_u32(32), 0);
    dev.write_u32(32, 0xffffffff);
    assert_eq!(
        dev.read_u32(32),
        VIRTIO_F_VERSION_1 | VIRTIO_F_ACCESS_PLATFORM | VIRTIO_F_RING_PACKED
    );
    // driver feature (bank 2)
    dev.write_u32(36, 2);
    assert_eq!(dev.read_u32(32), 0);
    dev.write_u32(32, 0xffffffff);
    assert_eq!(dev.read_u32(32), 0);
    // host notify
    assert_eq!(dev.read_u32(80), 0);
    // interrupt status
    assert_eq!(dev.read_u32(96), 0);
    // interrupt ACK (queue 0)
    assert_eq!(dev.read_u32(100), 0);
    // device status
    assert_eq!(dev.read_u32(112), 0);
    // config generation
    assert_eq!(dev.read_u32(0xfc), 0);

    // queue index
    assert_eq!(dev.read_u32(48), 0);
    // queue max size (queue 0)
    assert_eq!(dev.read_u32(52), 0x100);
    // queue size (queue 0)
    assert_eq!(dev.read_u32(56), 0x100);
    dev.write_u32(56, 0x20);
    assert_eq!(dev.read_u32(56), 0x20);
    // queue enable (queue 0)
    assert_eq!(dev.read_u32(68), 0);
    dev.write_u32(68, 1);
    assert_eq!(dev.read_u32(68), 1);
    dev.write_u32(68, 0xffffffff);
    assert_eq!(dev.read_u32(68), 1);
    dev.write_u32(68, 0);
    assert_eq!(dev.read_u32(68), 0);
    // queue descriptor address low (queue 0)
    assert_eq!(dev.read_u32(128), 0);
    dev.write_u32(128, 0xffff);
    assert_eq!(dev.read_u32(128), 0xffff);
    // queue descriptor address high (queue 0)
    assert_eq!(dev.read_u32(132), 0);
    dev.write_u32(132, 1);
    assert_eq!(dev.read_u32(132), 1);
    // queue available address low (queue 0)
    assert_eq!(dev.read_u32(144), 0);
    dev.write_u32(144, 0xeeee);
    assert_eq!(dev.read_u32(144), 0xeeee);
    // queue available address high (queue 0)
    assert_eq!(dev.read_u32(148), 0);
    dev.write_u32(148, 2);
    assert_eq!(dev.read_u32(148), 2);
    // queue used address low (queue 0)
    assert_eq!(dev.read_u32(160), 0);
    dev.write_u32(160, 0xdddd);
    assert_eq!(dev.read_u32(160), 0xdddd);
    // queue used address high (queue 0)
    assert_eq!(dev.read_u32(164), 0);
    dev.write_u32(164, 3);
    assert_eq!(dev.read_u32(164), 3);

    // switch to queue #1
    dev.write_u32(48, 1);
    assert_eq!(dev.read_u32(48), 1);
    // queue max size (queue 1)
    assert_eq!(dev.read_u32(52), 0);
    // queue size (queue 1)
    assert_eq!(dev.read_u32(56), 0);
    dev.write_u32(56, 2);
    assert_eq!(dev.read_u32(56), 0);
    // queue enable (queue 1)
    assert_eq!(dev.read_u32(68), 0);
    dev.write_u32(68, 1);
    assert_eq!(dev.read_u32(68), 0);
    // queue descriptor address low (queue 1)
    assert_eq!(dev.read_u32(128), 0);
    dev.write_u32(128, 1);
    assert_eq!(dev.read_u32(128), 0);
    // queue descriptor address high (queue 1)
    assert_eq!(dev.read_u32(132), 0);
    dev.write_u32(132, 1);
    assert_eq!(dev.read_u32(132), 0);
    // queue available address low (queue 1)
    assert_eq!(dev.read_u32(144), 0);
    dev.write_u32(144, 1);
    assert_eq!(dev.read_u32(144), 0);
    // queue available address high (queue 1)
    assert_eq!(dev.read_u32(148), 0);
    dev.write_u32(148, 1);
    assert_eq!(dev.read_u32(148), 0);
    // queue used address low (queue 1)
    assert_eq!(dev.read_u32(160), 0);
    dev.write_u32(160, 1);
    assert_eq!(dev.read_u32(160), 0);
    // queue used address high (queue 1)
    assert_eq!(dev.read_u32(164), 0);
    dev.write_u32(164, 1);
    assert_eq!(dev.read_u32(164), 0);
}

#[async_test]
async fn verify_pci_config(driver: DefaultDriver) {
    let mut pci_test_device =
        VirtioPciTestDevice::new(&driver, 1, &VirtioTestMemoryAccess::new(), None);
    let mut capabilities = 0;
    pci_test_device
        .pci_device
        .pci_cfg_read(4, &mut capabilities)
        .unwrap();
    assert_eq!(
        capabilities,
        (cfg_space::Status::new()
            .with_capabilities_list(true)
            .into_bits() as u32)
            << 16
    );
    let mut next_cap_offset = 0;
    pci_test_device
        .pci_device
        .pci_cfg_read(0x34, &mut next_cap_offset)
        .unwrap();
    assert_ne!(next_cap_offset, 0);

    let mut header = 0;
    pci_test_device
        .pci_device
        .pci_cfg_read(next_cap_offset as u16, &mut header)
        .unwrap();
    let header = header.to_le_bytes();
    assert_eq!(header[0], CapabilityId::MSIX.0);
    next_cap_offset = header[1] as u32;
    assert_ne!(next_cap_offset, 0);

    let mut header = 0;
    pci_test_device
        .pci_device
        .pci_cfg_read(next_cap_offset as u16, &mut header)
        .unwrap();
    let header = header.to_le_bytes();
    assert_eq!(header[0], CapabilityId::VENDOR_SPECIFIC.0);
    assert_eq!(header[3], VirtioPciCapType::COMMON_CFG.0);
    assert_eq!(header[2], 16);
    let mut buf = 0;

    pci_test_device
        .pci_device
        .pci_cfg_read(next_cap_offset as u16 + 4, &mut buf)
        .unwrap();
    assert_eq!(buf, 0);
    pci_test_device
        .pci_device
        .pci_cfg_read(next_cap_offset as u16 + 8, &mut buf)
        .unwrap();
    assert_eq!(buf, 0);
    pci_test_device
        .pci_device
        .pci_cfg_read(next_cap_offset as u16 + 12, &mut buf)
        .unwrap();
    assert_eq!(buf, 0x38);
    next_cap_offset = header[1] as u32;
    assert_ne!(next_cap_offset, 0);

    let mut header = 0;
    pci_test_device
        .pci_device
        .pci_cfg_read(next_cap_offset as u16, &mut header)
        .unwrap();
    let header = header.to_le_bytes();
    assert_eq!(header[0], CapabilityId::VENDOR_SPECIFIC.0);
    assert_eq!(header[3], VirtioPciCapType::NOTIFY_CFG.0);
    assert_eq!(header[2], 20);
    pci_test_device
        .pci_device
        .pci_cfg_read(next_cap_offset as u16 + 4, &mut buf)
        .unwrap();
    assert_eq!(buf, 0);
    pci_test_device
        .pci_device
        .pci_cfg_read(next_cap_offset as u16 + 8, &mut buf)
        .unwrap();
    assert_eq!(buf, 0x38);
    pci_test_device
        .pci_device
        .pci_cfg_read(next_cap_offset as u16 + 12, &mut buf)
        .unwrap();
    assert_eq!(buf, 4);
    next_cap_offset = header[1] as u32;
    assert_ne!(next_cap_offset, 0);

    let mut header = 0;
    pci_test_device
        .pci_device
        .pci_cfg_read(next_cap_offset as u16, &mut header)
        .unwrap();
    let header = header.to_le_bytes();
    assert_eq!(header[0], CapabilityId::VENDOR_SPECIFIC.0);
    assert_eq!(header[3], VirtioPciCapType::ISR_CFG.0);
    assert_eq!(header[2], 16);
    pci_test_device
        .pci_device
        .pci_cfg_read(next_cap_offset as u16 + 4, &mut buf)
        .unwrap();
    assert_eq!(buf, 0);
    pci_test_device
        .pci_device
        .pci_cfg_read(next_cap_offset as u16 + 8, &mut buf)
        .unwrap();
    assert_eq!(buf, 0x3c);
    pci_test_device
        .pci_device
        .pci_cfg_read(next_cap_offset as u16 + 12, &mut buf)
        .unwrap();
    assert_eq!(buf, 4);
    next_cap_offset = header[1] as u32;
    assert_ne!(next_cap_offset, 0);

    let mut header = 0;
    pci_test_device
        .pci_device
        .pci_cfg_read(next_cap_offset as u16, &mut header)
        .unwrap();
    let header = header.to_le_bytes();
    assert_eq!(header[0], CapabilityId::VENDOR_SPECIFIC.0);
    assert_eq!(header[3], VirtioPciCapType::DEVICE_CFG.0);
    assert_eq!(header[2], 16);
    pci_test_device
        .pci_device
        .pci_cfg_read(next_cap_offset as u16 + 4, &mut buf)
        .unwrap();
    assert_eq!(buf, 0);
    pci_test_device
        .pci_device
        .pci_cfg_read(next_cap_offset as u16 + 8, &mut buf)
        .unwrap();
    assert_eq!(buf, 0x40);
    pci_test_device
        .pci_device
        .pci_cfg_read(next_cap_offset as u16 + 12, &mut buf)
        .unwrap();
    assert_eq!(buf, 12);
    next_cap_offset = header[1] as u32;
    assert_eq!(next_cap_offset, 0);
}

#[async_test]
async fn verify_pci_registers(driver: DefaultDriver) {
    let mut pci_test_device =
        VirtioPciTestDevice::new(&driver, 1, &VirtioTestMemoryAccess::new(), None);
    let bar_address1: u64 = 0x2000000000;
    pci_test_device
        .pci_device
        .pci_cfg_write(0x14, (bar_address1 >> 32) as u32)
        .unwrap();
    pci_test_device
        .pci_device
        .pci_cfg_write(0x10, bar_address1 as u32)
        .unwrap();

    let bar_address2: u64 = 0x4000;
    pci_test_device
        .pci_device
        .pci_cfg_write(0x1c, (bar_address2 >> 32) as u32)
        .unwrap();
    pci_test_device
        .pci_device
        .pci_cfg_write(0x18, bar_address2 as u32)
        .unwrap();

    pci_test_device
        .pci_device
        .pci_cfg_write(
            0x4,
            cfg_space::Command::new()
                .with_mmio_enabled(true)
                .into_bits() as u32,
        )
        .unwrap();

    // device feature bank index
    assert_eq!(pci_test_device.read_u32(bar_address1), 0);
    // device feature (bank 0)
    assert_eq!(
        pci_test_device.read_u32(bar_address1 + 4),
        VIRTIO_F_RING_INDIRECT_DESC | VIRTIO_F_RING_EVENT_IDX | 2
    );
    // device feature (bank 1)
    pci_test_device.write_u32(bar_address1, 1);
    assert_eq!(pci_test_device.read_u32(bar_address1), 1);
    assert_eq!(
        pci_test_device.read_u32(bar_address1 + 4),
        VIRTIO_F_VERSION_1 | VIRTIO_F_ACCESS_PLATFORM | VIRTIO_F_RING_PACKED
    );
    // device feature (bank 2)
    pci_test_device.write_u32(bar_address1, 2);
    assert_eq!(pci_test_device.read_u32(bar_address1), 2);
    assert_eq!(pci_test_device.read_u32(bar_address1 + 4), 0);
    // driver feature bank index
    assert_eq!(pci_test_device.read_u32(bar_address1 + 8), 0);
    // driver feature (bank 0)
    assert_eq!(pci_test_device.read_u32(bar_address1 + 12), 0);
    pci_test_device.write_u32(bar_address1 + 12, 2);
    assert_eq!(pci_test_device.read_u32(bar_address1 + 12), 2);
    pci_test_device.write_u32(bar_address1 + 12, 0xffffffff);
    assert_eq!(
        pci_test_device.read_u32(bar_address1 + 12),
        VIRTIO_F_RING_INDIRECT_DESC | VIRTIO_F_RING_EVENT_IDX | 2
    );
    // driver feature (bank 1)
    pci_test_device.write_u32(bar_address1 + 8, 1);
    assert_eq!(pci_test_device.read_u32(bar_address1 + 8), 1);
    assert_eq!(pci_test_device.read_u32(bar_address1 + 12), 0);
    pci_test_device.write_u32(bar_address1 + 12, 0xffffffff);
    assert_eq!(
        pci_test_device.read_u32(bar_address1 + 12),
        VIRTIO_F_VERSION_1 | VIRTIO_F_ACCESS_PLATFORM | VIRTIO_F_RING_PACKED
    );
    // driver feature (bank 2)
    pci_test_device.write_u32(bar_address1 + 8, 2);
    assert_eq!(pci_test_device.read_u32(bar_address1 + 8), 2);
    assert_eq!(pci_test_device.read_u32(bar_address1 + 12), 0);
    pci_test_device.write_u32(bar_address1 + 12, 0xffffffff);
    assert_eq!(pci_test_device.read_u32(bar_address1 + 12), 0);
    // max queues and the msix vector for config changes
    assert_eq!(pci_test_device.read_u32(bar_address1 + 16), 1 << 16);
    // queue index, config generation and device status
    assert_eq!(pci_test_device.read_u32(bar_address1 + 20), 0);
    // current queue size and msix vector
    assert_eq!(pci_test_device.read_u32(bar_address1 + 24), 0x100);
    pci_test_device.write_u32(bar_address1 + 24, 0x20);
    assert_eq!(pci_test_device.read_u32(bar_address1 + 24), 0x20);
    // current queue enabled and notify offset
    assert_eq!(pci_test_device.read_u32(bar_address1 + 28), 0);
    pci_test_device.write_u32(bar_address1 + 28, 1);
    assert_eq!(pci_test_device.read_u32(bar_address1 + 28), 1);
    pci_test_device.write_u32(bar_address1 + 28, 0xffff);
    assert_eq!(pci_test_device.read_u32(bar_address1 + 28), 1);
    pci_test_device.write_u32(bar_address1 + 28, 0);
    assert_eq!(pci_test_device.read_u32(bar_address1 + 28), 0);
    // current queue descriptor table address (low)
    assert_eq!(pci_test_device.read_u32(bar_address1 + 32), 0);
    pci_test_device.write_u32(bar_address1 + 32, 0xffff);
    assert_eq!(pci_test_device.read_u32(bar_address1 + 32), 0xffff);
    // current queue descriptor table address (high)
    assert_eq!(pci_test_device.read_u32(bar_address1 + 36), 0);
    pci_test_device.write_u32(bar_address1 + 36, 1);
    assert_eq!(pci_test_device.read_u32(bar_address1 + 36), 1);
    // current queue available ring address (low)
    assert_eq!(pci_test_device.read_u32(bar_address1 + 40), 0);
    pci_test_device.write_u32(bar_address1 + 40, 0xeeee);
    assert_eq!(pci_test_device.read_u32(bar_address1 + 40), 0xeeee);
    // current queue available ring address (high)
    assert_eq!(pci_test_device.read_u32(bar_address1 + 44), 0);
    pci_test_device.write_u32(bar_address1 + 44, 2);
    assert_eq!(pci_test_device.read_u32(bar_address1 + 44), 2);
    // current queue used ring address (low)
    assert_eq!(pci_test_device.read_u32(bar_address1 + 48), 0);
    pci_test_device.write_u32(bar_address1 + 48, 0xdddd);
    assert_eq!(pci_test_device.read_u32(bar_address1 + 48), 0xdddd);
    // current queue used ring address (high)
    assert_eq!(pci_test_device.read_u32(bar_address1 + 52), 0);
    pci_test_device.write_u32(bar_address1 + 52, 3);
    assert_eq!(pci_test_device.read_u32(bar_address1 + 52), 3);
    // VIRTIO_PCI_CAP_NOTIFY_CFG notification register
    assert_eq!(pci_test_device.read_u32(bar_address1 + 56), 0);
    // VIRTIO_PCI_CAP_ISR_CFG register
    assert_eq!(pci_test_device.read_u32(bar_address1 + 60), 0);

    // switch to queue #1 (disabled, only one queue on this device)
    let queue_index: u16 = 1;
    pci_test_device
        .pci_device
        .mmio_write(bar_address1 + 22, &queue_index.to_le_bytes())
        .unwrap();
    assert_eq!(pci_test_device.read_u32(bar_address1 + 20), 1 << 16);
    // current queue size and msix vector
    assert_eq!(pci_test_device.read_u32(bar_address1 + 24), 0);
    pci_test_device.write_u32(bar_address1 + 24, 2);
    assert_eq!(pci_test_device.read_u32(bar_address1 + 24), 0);
    // current queue enabled and notify offset
    assert_eq!(pci_test_device.read_u32(bar_address1 + 28), 0);
    pci_test_device.write_u32(bar_address1 + 28, 1);
    assert_eq!(pci_test_device.read_u32(bar_address1 + 28), 0);
    // current queue descriptor table address (low)
    assert_eq!(pci_test_device.read_u32(bar_address1 + 32), 0);
    pci_test_device.write_u32(bar_address1 + 32, 0x10);
    assert_eq!(pci_test_device.read_u32(bar_address1 + 32), 0);
    // current queue descriptor table address (high)
    assert_eq!(pci_test_device.read_u32(bar_address1 + 36), 0);
    pci_test_device.write_u32(bar_address1 + 36, 0x10);
    assert_eq!(pci_test_device.read_u32(bar_address1 + 36), 0);
    // current queue available ring address (low)
    assert_eq!(pci_test_device.read_u32(bar_address1 + 40), 0);
    pci_test_device.write_u32(bar_address1 + 40, 0x10);
    assert_eq!(pci_test_device.read_u32(bar_address1 + 40), 0);
    // current queue available ring address (high)
    assert_eq!(pci_test_device.read_u32(bar_address1 + 44), 0);
    pci_test_device.write_u32(bar_address1 + 44, 0x10);
    assert_eq!(pci_test_device.read_u32(bar_address1 + 44), 0);
    // current queue used ring address (low)
    assert_eq!(pci_test_device.read_u32(bar_address1 + 48), 0);
    pci_test_device.write_u32(bar_address1 + 48, 0x10);
    assert_eq!(pci_test_device.read_u32(bar_address1 + 48), 0);
    // current queue used ring address (high)
    assert_eq!(pci_test_device.read_u32(bar_address1 + 52), 0);
    pci_test_device.write_u32(bar_address1 + 52, 0x10);
    assert_eq!(pci_test_device.read_u32(bar_address1 + 52), 0);
}

async fn verify_queue_simple_inner(mut guest: VirtioTestGuest) {
    let base_addr = guest.get_queue_descriptor_backing_memory_address(0);
    let (tx, mut rx) = mesh::mpsc_channel();
    let event = Event::new();
    let mut queues = guest.create_direct_queues(|i| {
        let tx = tx.clone();
        CreateDirectQueueParams {
            process_work: Box::new(
                move |queue: &mut VirtioQueue, work: VirtioQueueCallbackWork| {
                    assert_eq!(work.payload.len(), 1);
                    assert_eq!(work.payload[0].length, 0x1000);
                    match work.payload[0].address {
                        addr if addr == base_addr => queue.complete(work, 123),
                        addr if addr == base_addr + 0x1000 => queue.complete(work, 456),
                        _ => panic!("Unexpected address {}", work.payload[0].address),
                    }
                },
            ),
            notify: Interrupt::from_fn(move || {
                tx.send(i as usize);
            }),
            event: event.clone(),
        }
    });

    guest.add_to_avail_queue(0);
    guest.add_to_avail_queue(0);
    event.signal();
    must_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;
    let (desc, len) = guest.get_next_completed(0).unwrap();
    assert_eq!(desc, 0u16);
    assert_eq!(len, 123);
    let (desc, len) = match guest.get_next_completed(0) {
        Some(v) => v,
        None => {
            must_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;
            guest.get_next_completed(0).unwrap()
        }
    };
    assert_eq!(desc, 1u16);
    assert_eq!(len, 456);
    assert_eq!(guest.get_next_completed(0).is_none(), true);
    queues[0].stop().await;
}

#[async_test]
async fn verify_split_queue_simple(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    verify_queue_simple_inner(VirtioTestGuest::new_split(&driver, &test_mem, 1, 2, true)).await;
}
#[async_test]
async fn verify_packed_queue_simple(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    verify_queue_simple_inner(VirtioTestGuest::new_packed(&driver, &test_mem, 1, 2, true)).await;
}

async fn verify_queue_simple_interrupt_control_inner(mut guest: VirtioTestGuest, with_index: bool) {
    let (tx, mut rx) = mesh::mpsc_channel();
    let event = Event::new();
    let mut queues = guest.create_direct_queues(|i| {
        let tx = tx.clone();
        CreateDirectQueueParams {
            process_work: Box::new(
                move |queue: &mut VirtioQueue, work: VirtioQueueCallbackWork| {
                    assert_eq!(work.payload.len(), 1);
                    assert_eq!(work.payload[0].length, 0x1000);
                    queue.complete(work, 123);
                },
            ),
            notify: Interrupt::from_fn(move || {
                tx.send(i as usize);
            }),
            event: event.clone(),
        }
    });

    // interrupt on a specific descriptor
    if with_index {
        guest.enable_interrupt(0, Some(1));
        guest.add_to_avail_queue(0);
        event.signal();
        assert_no_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;
        guest.add_to_avail_queue(0);
        event.signal();
        must_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;

        let (_, len) = guest.get_next_completed(0).unwrap();
        assert_eq!(len, 123);
        let (_, len) = guest.get_next_completed(0).unwrap();
        assert_eq!(len, 123);
        assert_eq!(guest.get_next_completed(0).is_none(), true);
    }
    // always interrupt
    guest.enable_interrupt(0, None);
    guest.add_to_avail_queue(0);
    event.signal();
    must_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;
    let (_, len) = guest.get_next_completed(0).unwrap();
    assert_eq!(len, 123);
    assert_eq!(guest.get_next_completed(0).is_none(), true);

    // never interrupt
    guest.disable_interrupt(0);
    guest.add_to_avail_queue(0);
    guest.add_to_avail_queue(0);
    event.signal();
    assert_no_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;
    let (_, len) = guest.get_next_completed(0).unwrap();
    assert_eq!(len, 123);
    let (_, len) = guest.get_next_completed(0).unwrap();
    assert_eq!(len, 123);

    queues[0].stop().await;
}

#[async_test]
async fn verify_split_queue_simple_interrupt_control(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    verify_queue_simple_interrupt_control_inner(
        VirtioTestGuest::new_split(&driver, &test_mem, 1, 4, false),
        false,
    )
    .await;
}
#[async_test]
async fn verify_split_queue_simple_interrupt_control_with_index(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    verify_queue_simple_interrupt_control_inner(
        VirtioTestGuest::new_split(&driver, &test_mem, 1, 4, true),
        true,
    )
    .await;
}
#[async_test]
async fn verify_packed_queue_simple_interrupt_control(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    verify_queue_simple_interrupt_control_inner(
        VirtioTestGuest::new_packed(&driver, &test_mem, 1, 4, false),
        false,
    )
    .await;
}
#[async_test]
async fn verify_packed_queue_simple_interrupt_control_with_index(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    verify_queue_simple_interrupt_control_inner(
        VirtioTestGuest::new_packed(&driver, &test_mem, 1, 4, true),
        true,
    )
    .await;
}

async fn verify_queue_indirect_inner(mut guest: VirtioTestGuest) {
    let base_addr = guest.get_queue_descriptor_backing_memory_address(0);
    let (tx, mut rx) = mesh::mpsc_channel();
    let event = Event::new();
    let mut queues = guest.create_direct_queues(|i| {
        let tx = tx.clone();
        CreateDirectQueueParams {
            process_work: Box::new(
                move |queue: &mut VirtioQueue, work: VirtioQueueCallbackWork| {
                    assert_eq!(work.payload.len(), 1);
                    assert_eq!(work.payload[0].length, 0x1000);
                    match work.payload[0].address {
                        0xffffffff00000000u64 => queue.complete(work, 123),
                        addr if addr == base_addr + 0x1000 => queue.complete(work, 456),
                        _ => panic!("Unexpected address {}", work.payload[0].address),
                    }
                },
            ),
            notify: Interrupt::from_fn(move || {
                tx.send(i as usize);
            }),
            event: event.clone(),
        }
    });

    guest.add_indirect_to_avail_queue(0);
    guest.add_to_avail_queue(0);
    event.signal();
    must_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;
    let (desc, len) = guest.get_next_completed(0).unwrap();
    assert_eq!(desc, 0u16);
    assert_eq!(len, 123);
    let (desc, len) = match guest.get_next_completed(0) {
        Some(v) => v,
        None => {
            must_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;
            guest.get_next_completed(0).unwrap()
        }
    };
    assert_eq!(desc, 1u16);
    assert_eq!(len, 456);
    assert_eq!(guest.get_next_completed(0).is_none(), true);
    queues[0].stop().await;
}

#[async_test]
async fn verify_split_queue_indirect(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    verify_queue_indirect_inner(VirtioTestGuest::new_split(&driver, &test_mem, 1, 2, true)).await;
}
#[async_test]
async fn verify_packed_queue_indirect(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    verify_queue_indirect_inner(VirtioTestGuest::new_packed(&driver, &test_mem, 1, 2, true)).await;
}

async fn verify_queue_linked_inner(mut guest: VirtioTestGuest) {
    let (tx, mut rx) = mesh::mpsc_channel();
    let base_address = guest.get_queue_descriptor_backing_memory_address(0);
    let event = Event::new();
    let mut queues = guest.create_direct_queues(|i| {
        let tx = tx.clone();
        CreateDirectQueueParams {
            process_work: Box::new(
                move |queue: &mut VirtioQueue, work: VirtioQueueCallbackWork| {
                    if work.payload.len() == 3 {
                        for i in 0..work.payload.len() {
                            assert_eq!(work.payload[i].address, base_address + 0x1000 * i as u64);
                            assert_eq!(work.payload[i].length, 0x1000);
                        }
                        queue.complete(work, 123);
                    } else {
                        assert_eq!(work.payload.len(), 1);
                        queue.complete(work, 456);
                    }
                },
            ),
            notify: Interrupt::from_fn(move || {
                tx.send(i as usize);
            }),
            event: event.clone(),
        }
    });

    guest.add_linked_to_avail_queue(0, 3);
    guest.add_to_avail_queue(0);
    event.signal();
    must_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;
    let (desc, len) = guest.get_next_completed(0).unwrap();
    assert_eq!(desc, 0u16);
    assert_eq!(len, 123);
    let (desc, len) = match guest.get_next_completed(0) {
        Some(v) => v,
        None => {
            must_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;
            guest.get_next_completed(0).unwrap()
        }
    };
    assert_eq!(desc, 3u16);
    assert_eq!(len, 456);
    assert_eq!(guest.get_next_completed(0).is_none(), true);
    queues[0].stop().await;
}

#[async_test]
async fn verify_split_queue_linked(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    verify_queue_linked_inner(VirtioTestGuest::new_split(&driver, &test_mem, 1, 8, true)).await;
}
#[async_test]
async fn verify_packed_queue_linked(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    verify_queue_linked_inner(VirtioTestGuest::new_packed(&driver, &test_mem, 1, 8, true)).await;
}
/// A packed linked chain that starts near the end of the ring and wraps to
/// the beginning (indices 2 → 3 → 0 in a queue_size=4 ring). Without
/// correct index wrapping in `descriptor()`, the NEXT index after 3 would
/// be 4 (out of bounds) instead of 0.
#[async_test]
async fn verify_packed_queue_linked_wrapping(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    let queue_size = 4u16;
    let mut guest = VirtioTestGuest::new_packed(&driver, &test_mem, 1, queue_size, true);

    let (tx, mut rx) = mesh::mpsc_channel();
    let event = Event::new();
    let mut queues = guest.create_direct_queues(|i| {
        let tx = tx.clone();
        CreateDirectQueueParams {
            process_work: Box::new(
                move |queue: &mut VirtioQueue, work: VirtioQueueCallbackWork| {
                    let len = work.payload.len() as u32;
                    queue.complete(work, len);
                },
            ),
            notify: Interrupt::from_fn(move || {
                tx.send(i as usize);
            }),
            event: event.clone(),
        }
    });

    // Submit and complete 2 single descriptors to advance the packed ring
    // cursor to index 2.
    for _ in 0..2 {
        guest.add_to_avail_queue(0);
        event.signal();
        must_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;
        guest.get_next_completed(0).unwrap();
    }

    // Submit a 3-descriptor linked chain starting at index 2.
    // The chain wraps: 2 → 3 → 0.
    guest.add_linked_to_avail_queue(0, 3);
    event.signal();
    must_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;
    let (desc, len) = guest.get_next_completed(0).unwrap();
    assert_eq!(desc, 2);
    assert_eq!(len, 3);

    queues[0].stop().await;
}

/// Verify that packed indirect descriptors ignore the NEXT flag. Per the
/// virtio spec, packed indirect descriptors consume all entries based on
/// the buffer length, not the NEXT flag. If the device checks NEXT before
/// checking `active_indirect_len`, a stale or malicious NEXT flag on the
/// last indirect descriptor could cause out-of-bounds access or incorrect
/// chaining using the primary ring size instead of the indirect table size.
#[async_test]
async fn verify_packed_indirect_ignores_next_flag(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    let queue_size = 8u16;
    let mut guest = VirtioTestGuest::new_packed(&driver, &test_mem, 1, queue_size, true);

    let (tx, mut rx) = mesh::mpsc_channel();
    let event = Event::new();
    let mut queues = guest.create_direct_queues(|i| {
        let tx = tx.clone();
        CreateDirectQueueParams {
            process_work: Box::new(
                move |queue: &mut VirtioQueue, work: VirtioQueueCallbackWork| {
                    let len = work.payload.len() as u32;
                    queue.complete(work, len);
                },
            ),
            notify: Interrupt::from_fn(move || {
                tx.send(i as usize);
            }),
            event: event.clone(),
        }
    });

    // Submit an indirect descriptor chain of 3 entries, but set the NEXT
    // flag on the last indirect descriptor. The device should still see
    // exactly 3 payload entries, because packed indirect tables are sized
    // by buffer length, not the NEXT flag.
    let buffer_addr = guest.get_queue_descriptor_backing_memory_address(0);
    let indirect_count: u16 = 3;
    for i in 0..indirect_count {
        let base = buffer_addr + 0x10 * i as u64;
        let indirect_buffer_addr = 0xffffffff00000000u64 + 0x1000 * i as u64;
        // physical address
        test_mem.modify_memory_map(base, &indirect_buffer_addr.to_le_bytes(), false);
        // length
        test_mem.modify_memory_map(base + 8, &0x1000u32.to_le_bytes(), false);
        // buffer_id (ignored for indirect)
        test_mem.modify_memory_map(base + 12, &0u16.to_le_bytes(), false);
        // Set the NEXT flag on ALL indirect descriptors, including the last.
        // The device must ignore this flag for packed indirect descriptors.
        let flags = DescriptorFlags::new().with_next(true);
        test_mem.modify_memory_map(base + 14, &flags.into_bits().to_le_bytes(), false);
    }

    guest.make_packed_descriptors_available(
        0,
        vec![DescriptorFlags::new().with_indirect(true)],
        Some(indirect_count as u32),
    );
    event.signal();
    must_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;
    let (desc, len) = guest.get_next_completed(0).unwrap();
    assert_eq!(desc, 0);
    // Must be exactly 3 payload entries (not more from following NEXT).
    assert_eq!(len, 3);

    queues[0].stop().await;
}

/// Verify that packed queues work with a non-power-of-two queue size.
/// The virtio spec (§2.8.10.1) does not require packed queue sizes to be
/// powers of two, unlike split queues (§2.7.1).
#[async_test]
async fn verify_packed_queue_non_power_of_two(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    let queue_size = 6u16;
    let mut guest = VirtioTestGuest::new_packed(&driver, &test_mem, 1, queue_size, true);

    let (tx, mut rx) = mesh::mpsc_channel();
    let event = Event::new();
    let mut queues = guest.create_direct_queues(|i| {
        let tx = tx.clone();
        CreateDirectQueueParams {
            process_work: Box::new(
                move |queue: &mut VirtioQueue, work: VirtioQueueCallbackWork| {
                    let len = work.payload.len() as u32;
                    queue.complete(work, len);
                },
            ),
            notify: Interrupt::from_fn(move || {
                tx.send(i as usize);
            }),
            event: event.clone(),
        }
    });

    // Submit and complete 4 single descriptors to advance the cursor to
    // index 4.
    for _ in 0..4 {
        guest.add_to_avail_queue(0);
        event.signal();
        must_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;
        guest.get_next_completed(0).unwrap();
    }

    // Submit a 3-descriptor linked chain starting at index 4.
    // The chain wraps: 4 → 5 → 0.
    guest.add_linked_to_avail_queue(0, 3);
    event.signal();
    must_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;
    let (desc, len) = guest.get_next_completed(0).unwrap();
    assert_eq!(desc, 4);
    assert_eq!(len, 3);

    queues[0].stop().await;
}

async fn verify_queue_indirect_linked_inner(mut guest: VirtioTestGuest) {
    let (tx, mut rx) = mesh::mpsc_channel();
    let event = Event::new();
    let mut queues = guest.create_direct_queues(|i| {
        let tx = tx.clone();
        CreateDirectQueueParams {
            process_work: Box::new(
                move |queue: &mut VirtioQueue, work: VirtioQueueCallbackWork| {
                    if work.payload.len() == 3 {
                        for i in 0..work.payload.len() {
                            assert_eq!(
                                work.payload[i].address,
                                0xffffffff00000000u64 + 0x1000 * i as u64
                            );
                            assert_eq!(work.payload[i].length, 0x1000);
                        }
                        queue.complete(work, 123);
                    } else {
                        assert_eq!(work.payload.len(), 1);
                        queue.complete(work, 456);
                    }
                },
            ),
            notify: Interrupt::from_fn(move || {
                tx.send(i as usize);
            }),
            event: event.clone(),
        }
    });

    guest.add_indirect_linked_to_avail_queue(0, 3);
    guest.add_to_avail_queue(0);
    event.signal();
    must_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;
    let (desc, len) = guest.get_next_completed(0).unwrap();
    assert_eq!(desc, 0u16);
    assert_eq!(len, 123);
    let (desc, len) = match guest.get_next_completed(0) {
        Some(v) => v,
        None => {
            must_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;
            guest.get_next_completed(0).unwrap()
        }
    };
    assert_eq!(desc, 1u16);
    assert_eq!(len, 456);
    assert_eq!(guest.get_next_completed(0).is_none(), true);
    queues[0].stop().await;
}

#[async_test]
async fn verify_split_queue_indirect_linked(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    verify_queue_indirect_linked_inner(VirtioTestGuest::new_split(&driver, &test_mem, 1, 8, true))
        .await;
}
#[async_test]
async fn verify_packed_queue_indirect_linked(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    verify_queue_indirect_linked_inner(VirtioTestGuest::new_packed(&driver, &test_mem, 1, 8, true))
        .await;
}

async fn verify_queue_avail_rollover_inner(mut guest: VirtioTestGuest) {
    let base_addr = guest.get_queue_descriptor_backing_memory_address(0);
    let (tx, mut rx) = mesh::mpsc_channel();
    let event = Event::new();
    let mut queues = guest.create_direct_queues(|i| {
        let tx = tx.clone();
        CreateDirectQueueParams {
            process_work: Box::new(
                move |queue: &mut VirtioQueue, work: VirtioQueueCallbackWork| {
                    assert_eq!(work.payload.len(), 1);
                    assert_eq!(work.payload[0].length, 0x1000);
                    if work.payload[0].address == base_addr {
                        queue.complete(work, 123);
                    } else if work.payload[0].address == base_addr + 0x1000 {
                        queue.complete(work, 456);
                    } else {
                        panic!(
                            "Unexpected descriptor address {:x}",
                            work.payload[0].address
                        );
                    }
                },
            ),
            notify: Interrupt::from_fn(move || {
                tx.send(i as usize);
            }),
            event: event.clone(),
        }
    });

    for _ in 0..3 {
        guest.add_to_avail_queue(0);
        event.signal();
        must_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;
        let (desc, len) = guest.get_next_completed(0).unwrap();
        if desc == 0 {
            assert_eq!(len, 123);
        } else if desc == 1 {
            assert_eq!(len, 456);
        } else {
            panic!("Unexpected descriptor index");
        }
        assert_eq!(guest.get_next_completed(0).is_none(), true);
    }

    queues[0].stop().await;
}

#[async_test]
async fn verify_split_queue_avail_rollover(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    verify_queue_avail_rollover_inner(VirtioTestGuest::new_split(&driver, &test_mem, 1, 2, true))
        .await;
}
#[async_test]
async fn verify_packed_queue_avail_rollover(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    verify_queue_avail_rollover_inner(VirtioTestGuest::new_packed(&driver, &test_mem, 1, 2, true))
        .await;
}

async fn verify_multi_queue_inner(mut guest: VirtioTestGuest) {
    let (tx, mut rx) = mesh::mpsc_channel();
    let events = (0..guest.num_queues)
        .map(|_| Event::new())
        .collect::<Vec<_>>();
    let mut queues = guest.create_direct_queues(|queue_index| {
        let tx = tx.clone();
        let base_addr = guest.get_queue_descriptor_backing_memory_address(queue_index);
        CreateDirectQueueParams {
            process_work: Box::new(
                move |queue: &mut VirtioQueue, work: VirtioQueueCallbackWork| {
                    assert_eq!(work.payload.len(), 1);
                    assert_eq!(work.payload[0].address, base_addr);
                    assert_eq!(work.payload[0].length, 0x1000);
                    queue.complete(work, 123 * queue_index as u32);
                },
            ),
            notify: Interrupt::from_fn(move || {
                tx.send(queue_index as usize);
            }),
            event: events[queue_index as usize].clone(),
        }
    });

    for (i, event) in events.iter().enumerate() {
        let queue_index = i as u16;
        guest.add_to_avail_queue(queue_index);
        event.signal();
    }
    // wait for all queue processing to finish
    for _ in 0..guest.num_queues {
        must_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;
    }
    // check results
    for queue_index in 0..guest.num_queues {
        let (desc, len) = guest.get_next_completed(queue_index).unwrap();
        assert_eq!(desc, 0u16);
        assert_eq!(len, 123 * queue_index as u32);
    }
    // verify no extraneous completions
    for (i, queue) in queues.iter_mut().enumerate() {
        let queue_index = i as u16;
        assert_eq!(guest.get_next_completed(queue_index).is_none(), true);
        queue.stop().await;
    }
}

#[async_test]
async fn verify_split_multi_queue(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    let guest = VirtioTestGuest::new_split(&driver, &test_mem, 5, 2, true);
    verify_multi_queue_inner(guest).await;
}
#[async_test]
async fn verify_packed_multi_queue(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    let guest = VirtioTestGuest::new_packed(&driver, &test_mem, 5, 2, true);
    verify_multi_queue_inner(guest).await;
}

fn take_mmio_interrupt_status(dev: &mut VirtioMmioDevice, mask: u32) -> u32 {
    let mut v = [0; 4];
    dev.mmio_read(96, &mut v).unwrap();
    dev.mmio_write(100, &mask.to_ne_bytes()).unwrap();
    u32::from_ne_bytes(v)
}

async fn expect_mmio_interrupt(
    dev: &mut VirtioMmioDevice,
    target: &TestLineInterruptTarget,
    mask: u32,
    multiple_expected: bool,
) {
    poll_fn(|cx| target.poll_high(cx, 0)).await;
    let v = take_mmio_interrupt_status(dev, mask);
    assert_eq!(v & mask, mask);
    assert!(multiple_expected || !target.is_high(0));
}

async fn verify_device_queue_simple_inner(
    test_mem: Arc<VirtioTestMemoryAccess>,
    mut guest: VirtioTestGuest,
    features: VirtioDeviceFeatures,
) {
    let doorbell_registration: Arc<dyn DoorbellRegistration> = test_mem.clone();
    let mem = guest.mem();
    let target = TestLineInterruptTarget::new_arc();
    let interrupt = LineInterrupt::new_with_target("test", target.clone(), 0);
    let base_addr = guest.get_queue_descriptor_backing_memory_address(0);
    let queue_work = Arc::new(
        move |_: u16, queue: &mut VirtioQueue, work: VirtioQueueCallbackWork| {
            assert_eq!(work.payload.len(), 1);
            assert_eq!(work.payload[0].address, base_addr);
            assert_eq!(work.payload[0].length, 0x1000);
            queue.complete(work, 123);
        },
    );
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(guest.driver()));
    let mut dev = VirtioMmioDevice::new(
        Box::new(TestDevice::new(
            &driver_source,
            DeviceTraits {
                device_id: VirtioDeviceType::CONSOLE,
                device_features: features,
                max_queues: 1,
                device_register_length: 0,
                ..Default::default()
            },
            Some(queue_work),
        )),
        &driver_source.simple(),
        mem.clone(),
        interrupt,
        Some(doorbell_registration),
        0,
        1,
    )
    .unwrap();

    guest.setup_chipset_device(&mut dev, features).await;
    expect_mmio_interrupt(
        &mut dev,
        &target,
        VIRTIO_MMIO_INTERRUPT_STATUS_CONFIG_CHANGE,
        false,
    )
    .await;
    guest.add_to_avail_queue(0);
    // notify device
    dev.write_u32(80, 0);
    expect_mmio_interrupt(
        &mut dev,
        &target,
        VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER,
        false,
    )
    .await;
    let (desc, len) = guest.get_next_completed(0).unwrap();
    assert_eq!(desc, 0u16);
    assert_eq!(len, 123);
    assert_eq!(guest.get_next_completed(0).is_none(), true);
    // reset the device
    dev.write_u32(112, 0);
    drop(dev);
}

#[async_test]
async fn verify_device_split_queue_simple(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    let guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, 2, true);
    let features = VirtioDeviceFeatures::new()
        .with_bank(0, VIRTIO_F_RING_EVENT_IDX | 2)
        .with_bank(1, VIRTIO_F_VERSION_1);
    verify_device_queue_simple_inner(test_mem, guest, features).await;
}
#[async_test]
async fn verify_device_packed_queue_simple(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    let guest = VirtioTestGuest::new_packed(&driver, &test_mem, 1, 2, true);
    let features = VirtioDeviceFeatures::new()
        .with_bank(0, VIRTIO_F_RING_EVENT_IDX | 2)
        .with_bank(1, VIRTIO_F_VERSION_1 | VIRTIO_F_RING_PACKED);
    verify_device_queue_simple_inner(test_mem, guest, features).await;
}

async fn verify_device_multi_queue_inner(
    test_mem: Arc<VirtioTestMemoryAccess>,
    mut guest: VirtioTestGuest,
    num_queues: u16,
    features: VirtioDeviceFeatures,
) {
    let doorbell_registration: Arc<dyn DoorbellRegistration> = test_mem.clone();
    let mem = guest.mem();
    let target = TestLineInterruptTarget::new_arc();
    let interrupt = LineInterrupt::new_with_target("test", target.clone(), 0);
    let base_addr: Vec<_> = (0..num_queues)
        .map(|i| guest.get_queue_descriptor_backing_memory_address(i))
        .collect();
    let queue_work = Arc::new(
        move |i: u16, queue: &mut VirtioQueue, work: VirtioQueueCallbackWork| {
            assert_eq!(work.payload.len(), 1);
            assert_eq!(work.payload[0].address, base_addr[i as usize]);
            assert_eq!(work.payload[0].length, 0x1000);
            queue.complete(work, 123 * i as u32);
        },
    );
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(guest.driver()));
    let mut dev = VirtioMmioDevice::new(
        Box::new(TestDevice::new(
            &driver_source,
            DeviceTraits {
                device_id: VirtioDeviceType::CONSOLE,
                device_features: features,
                max_queues: num_queues + 1,
                device_register_length: 0,
                ..Default::default()
            },
            Some(queue_work),
        )),
        &driver_source.simple(),
        mem.clone(),
        interrupt,
        Some(doorbell_registration),
        0,
        1,
    )
    .unwrap();
    guest.setup_chipset_device(&mut dev, features).await;
    expect_mmio_interrupt(
        &mut dev,
        &target,
        VIRTIO_MMIO_INTERRUPT_STATUS_CONFIG_CHANGE,
        false,
    )
    .await;
    for i in 0..num_queues {
        guest.add_to_avail_queue(i);
        // notify device
        dev.write_u32(80, i as u32);
    }
    // check results
    for i in 0..num_queues {
        let (desc, len) = loop {
            if let Some(x) = guest.get_next_completed(i) {
                break x;
            }
            expect_mmio_interrupt(
                &mut dev,
                &target,
                VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER,
                i < (num_queues - 1),
            )
            .await;
        };
        assert_eq!(desc, 0u16);
        assert_eq!(len, 123 * i as u32);
    }
    // verify no extraneous completions
    for i in 0..num_queues {
        assert_eq!(guest.get_next_completed(i).is_none(), true);
    }
    // reset the device
    dev.write_u32(112, 0);
    drop(dev);
}

#[async_test]
async fn verify_device_split_multi_queue(driver: DefaultDriver) {
    let num_queues = 5;
    let test_mem = VirtioTestMemoryAccess::new();
    let guest = VirtioTestGuest::new_split(&driver, &test_mem, num_queues, 2, true);
    let features = VirtioDeviceFeatures::new()
        .with_bank(0, VIRTIO_F_RING_EVENT_IDX | 2)
        .with_bank(1, VIRTIO_F_VERSION_1);
    verify_device_multi_queue_inner(test_mem, guest, num_queues, features).await;
}
#[async_test]
async fn verify_device_packed_multi_queue(driver: DefaultDriver) {
    let num_queues = 5;
    let test_mem = VirtioTestMemoryAccess::new();
    let guest = VirtioTestGuest::new_packed(&driver, &test_mem, num_queues, 2, true);
    let features = VirtioDeviceFeatures::new()
        .with_bank(0, VIRTIO_F_RING_EVENT_IDX | 2)
        .with_bank(1, VIRTIO_F_VERSION_1 | VIRTIO_F_RING_PACKED);
    verify_device_multi_queue_inner(test_mem, guest, num_queues, features).await;
}

async fn verify_device_multi_queue_pci_inner(
    test_mem: Arc<VirtioTestMemoryAccess>,
    mut guest: VirtioTestGuest,
    num_queues: u16,
    features: VirtioDeviceFeatures,
) {
    let driver = guest.driver();
    let base_addr: Vec<_> = (0..num_queues)
        .map(|i| guest.get_queue_descriptor_backing_memory_address(i))
        .collect();
    let mut dev = VirtioPciTestDevice::new(
        &driver,
        num_queues + 1,
        &test_mem,
        Some(Arc::new(move |i, queue: &mut VirtioQueue, work| {
            assert_eq!(work.payload.len(), 1);
            assert_eq!(work.payload[0].address, base_addr[i as usize]);
            assert_eq!(work.payload[0].length, 0x1000);
            queue.complete(work, 123 * i as u32);
        })),
    );

    guest.setup_pci_device(&mut dev, features).await;

    let mut timer = PolledTimer::new(&driver);

    // expect a config generation interrupt
    timer.sleep(Duration::from_millis(100)).await;
    let delivered = dev.test_intc.get_next_interrupt().unwrap();
    assert_eq!(delivered.0, 0);
    assert!(dev.test_intc.get_next_interrupt().is_none());

    for i in 0..num_queues {
        guest.add_to_avail_queue(i);
        // notify device
        dev.write_u32(0x10000000000 + 0x38, i as u32);
    }
    // verify all queue processing finished
    timer.sleep(Duration::from_millis(100)).await;
    for _ in 0..num_queues {
        let delivered = dev.test_intc.get_next_interrupt();
        assert!(delivered.is_some());
    }
    // check results
    for i in 0..num_queues {
        let (desc, len) = guest.get_next_completed(i).unwrap();
        assert_eq!(desc, 0u16);
        assert_eq!(len, 123 * i as u32);
    }
    // verify no extraneous completions
    for i in 0..num_queues {
        assert_eq!(guest.get_next_completed(i).is_none(), true);
    }
    // reset the device (use write_u32 to bypass deferred IO)
    let current = dev.pci_device.read_u32(20);
    dev.pci_device.write_u32(20, current & !0xff);
    drop(dev);
}

#[async_test]
async fn verify_device_split_multi_queue_pci(driver: DefaultDriver) {
    let num_queues = 5;
    let test_mem = VirtioTestMemoryAccess::new();
    let guest = VirtioTestGuest::new_split(&driver, &test_mem, num_queues, 2, true);
    let features = VirtioDeviceFeatures::new()
        .with_bank(0, VIRTIO_F_RING_EVENT_IDX | 2)
        .with_bank(1, VIRTIO_F_VERSION_1);
    verify_device_multi_queue_pci_inner(test_mem, guest, num_queues, features).await;
}
#[async_test]
async fn verify_device_packed_multi_queue_pci(driver: DefaultDriver) {
    let num_queues = 5;
    let test_mem = VirtioTestMemoryAccess::new();
    let guest = VirtioTestGuest::new_packed(&driver, &test_mem, num_queues, 2, true);
    let features = VirtioDeviceFeatures::new()
        .with_bank(0, VIRTIO_F_RING_INDIRECT_DESC | VIRTIO_F_RING_EVENT_IDX | 2)
        .with_bank(1, VIRTIO_F_VERSION_1 | VIRTIO_F_RING_PACKED);
    verify_device_multi_queue_pci_inner(test_mem, guest, num_queues, features).await;
}

#[async_test]
async fn verify_enable_failure_mmio_does_not_set_driver_ok(_driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    let doorbell_registration: Arc<dyn DoorbellRegistration> = test_mem.clone();
    let interrupt = LineInterrupt::detached();

    let mut dev = VirtioMmioDevice::new(
        Box::new(FailingTestDevice {
            traits: DeviceTraits {
                device_id: VirtioDeviceType::CONSOLE,
                device_features: VirtioDeviceFeatures::new()
                    .with_bank(0, 2 | VIRTIO_F_RING_INDIRECT_DESC | VIRTIO_F_RING_EVENT_IDX),
                max_queues: 1,
                device_register_length: 0,
                ..Default::default()
            },
        }),
        &_driver,
        GuestMemory::empty(),
        interrupt,
        Some(doorbell_registration),
        0,
        1,
    )
    .unwrap();

    // Drive through ACKNOWLEDGE -> DRIVER -> FEATURES_OK -> DRIVER_OK
    dev.write_u32(112, VIRTIO_ACKNOWLEDGE);
    dev.write_u32(112, VIRTIO_DRIVER);
    dev.write_u32(36, 0);
    dev.write_u32(32, 2); // select matching features
    dev.write_u32(36, 1);
    dev.write_u32(32, VIRTIO_F_VERSION_1);
    dev.write_u32(112, VIRTIO_FEATURES_OK);

    // Set up one queue
    dev.write_u32(48, 0); // queue select
    dev.write_u32(56, 16); // queue size
    dev.write_u32(68, 1); // queue enable

    // Attempt DRIVER_OK — enable() will fail
    dev.write_u32(112, VIRTIO_DRIVER_OK);
    yield_and_poll_device(&mut dev).await;

    // Device status should NOT have DRIVER_OK set
    let status = dev.read_u32(112);
    assert_eq!(
        status & VIRTIO_DRIVER_OK,
        0,
        "DRIVER_OK must not be set when enable() fails"
    );
}

#[async_test]
async fn verify_enable_failure_pci_does_not_set_driver_ok(_driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    let doorbell_registration: Arc<dyn DoorbellRegistration> = test_mem.clone();
    let msi_conn = MsiConnection::new(AssignedBusRange::new(), 0);

    let mut dev = VirtioPciDevice::new(
        Box::new(FailingTestDevice {
            traits: DeviceTraits {
                device_id: VirtioDeviceType::CONSOLE,
                device_features: VirtioDeviceFeatures::new()
                    .with_bank(0, 2 | VIRTIO_F_RING_INDIRECT_DESC | VIRTIO_F_RING_EVENT_IDX),
                max_queues: 1,
                device_register_length: 12,
                ..Default::default()
            },
        }),
        &_driver,
        GuestMemory::empty(),
        PciInterruptModel::Msix(msi_conn.target()),
        Some(doorbell_registration),
        &mut ExternallyManagedMmioIntercepts,
        None,
    )
    .unwrap();

    let bar_address1: u64 = 0x10000000000;
    dev.pci_cfg_write(0x14, (bar_address1 >> 32) as u32)
        .unwrap();
    dev.pci_cfg_write(0x10, bar_address1 as u32).unwrap();

    let bar_address2: u64 = 0x20000000000;
    dev.pci_cfg_write(0x1c, (bar_address2 >> 32) as u32)
        .unwrap();
    dev.pci_cfg_write(0x18, bar_address2 as u32).unwrap();

    dev.pci_cfg_write(
        0x4,
        cfg_space::Command::new()
            .with_mmio_enabled(true)
            .into_bits() as u32,
    )
    .unwrap();

    // Drive through ACKNOWLEDGE -> DRIVER -> FEATURES_OK
    let mut buf = [0u8; 1];
    buf[0] = VIRTIO_ACKNOWLEDGE as u8;
    dev.mmio_write(bar_address1 + 20, &buf).unwrap();
    buf[0] = VIRTIO_DRIVER as u8;
    dev.mmio_write(bar_address1 + 20, &buf).unwrap();
    // Select features
    let mut val;
    val = 0u32.to_le_bytes();
    dev.mmio_write(bar_address1 + 8, &val).unwrap();
    val = 2u32.to_le_bytes();
    dev.mmio_write(bar_address1 + 12, &val).unwrap();
    val = 1u32.to_le_bytes();
    dev.mmio_write(bar_address1 + 8, &val).unwrap();
    val = VIRTIO_F_VERSION_1.to_le_bytes();
    dev.mmio_write(bar_address1 + 12, &val).unwrap();
    buf[0] = VIRTIO_FEATURES_OK as u8;
    dev.mmio_write(bar_address1 + 20, &buf).unwrap();

    // Set up queue 0
    dev.mmio_write(bar_address1 + 22, &0u16.to_le_bytes())
        .unwrap(); // queue select
    dev.mmio_write(bar_address1 + 24, &16u16.to_le_bytes())
        .unwrap(); // queue size
    // Set up MSI for the queue
    dev.mmio_write(bar_address2, &0u64.to_le_bytes()).unwrap();
    dev.mmio_write(bar_address2 + 8, &0u32.to_le_bytes())
        .unwrap();
    dev.mmio_write(bar_address2 + 12, &0u32.to_le_bytes())
        .unwrap();
    let msix_vector: u16 = 1;
    let msix_addr = bar_address2 + 0x10 * msix_vector as u64;
    dev.mmio_write(msix_addr, &(msix_vector as u64).to_le_bytes())
        .unwrap();
    dev.mmio_write(msix_addr + 8, &0u32.to_le_bytes()).unwrap();
    dev.mmio_write(msix_addr + 12, &0u32.to_le_bytes()).unwrap();
    dev.mmio_write(bar_address1 + 26, &msix_vector.to_le_bytes())
        .unwrap();
    // Enable queue
    dev.mmio_write(bar_address1 + 28, &1u16.to_le_bytes())
        .unwrap();
    // Enable all MSI interrupts
    dev.pci_cfg_write(0x40, 0x80000000).unwrap();

    // Attempt DRIVER_OK — enable() will fail (use write_u32 to bypass deferred IO)
    let current = dev.read_u32(20);
    dev.write_u32(20, (current & !0xff) | VIRTIO_DRIVER_OK);
    yield_and_poll_device(&mut dev).await;

    // Read back device status
    let status = dev.read_u32(20) & 0xff;
    assert_eq!(
        status & VIRTIO_DRIVER_OK,
        0,
        "DRIVER_OK must not be set when enable() fails"
    );
}

/// A linked chain using all queue_size descriptors (last has no NEXT flag)
/// must succeed — this is the maximum valid chain length per spec §2.7.5.3.1.
#[async_test]
async fn verify_chain_at_queue_size_succeeds(driver: DefaultDriver) {
    let queue_size: u16 = 4;
    let test_mem = VirtioTestMemoryAccess::new();
    let mut guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, queue_size, true);
    let (tx, mut rx) = mesh::mpsc_channel();
    let event = Event::new();
    let mut queues = guest.create_direct_queues(|i| {
        let tx = tx.clone();
        CreateDirectQueueParams {
            process_work: Box::new(
                move |queue: &mut VirtioQueue, work: VirtioQueueCallbackWork| {
                    assert_eq!(work.payload.len(), queue_size as usize);
                    queue.complete(work, 42);
                },
            ),
            notify: Interrupt::from_fn(move || {
                tx.send(i as usize);
            }),
            event: event.clone(),
        }
    });

    guest.add_linked_to_avail_queue(0, queue_size);
    event.signal();
    must_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;
    let (desc, len) = guest.get_next_completed(0).unwrap();
    assert_eq!(desc, 0u16);
    assert_eq!(len, 42);
    queues[0].stop().await;
}

/// A descriptor chain that forms a cycle (all queue_size descriptors point to
/// the next, with the last wrapping back to the first) must be rejected.
#[async_test]
async fn verify_chain_cycle_rejected(driver: DefaultDriver) {
    let queue_size: u16 = 4;
    let test_mem = VirtioTestMemoryAccess::new();
    let mut guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, queue_size, true);
    let event = Event::new();
    let queue_event = PolledWait::new(&driver, event.clone()).unwrap();
    let mut queue = VirtioQueue::new(
        guest.queue_features(),
        guest.queue_params(0),
        guest.mem(),
        Interrupt::from_fn(|| {}),
        queue_event,
        None,
    )
    .unwrap();

    // Manually build a cycle: desc 0 → 1 → 2 → 3 → 0 (all with NEXT flag).
    for i in 0..queue_size {
        let base = guest.get_queue_descriptor(0, i);
        let flags = u16::from(DescriptorFlags::new().with_next(true));
        test_mem.modify_memory_map(base + 12, &flags.to_le_bytes(), false);
        let next = (i + 1) % queue_size;
        test_mem.modify_memory_map(base + 14, &next.to_le_bytes(), false);
    }
    guest.queue_available_desc(0, 0);

    let result = queue.try_next();
    assert!(result.is_err(), "Cyclic chain must produce an error");
}

/// An indirect descriptor table with more entries than queue_size must be
/// clamped to queue_size (spec §2.7.5.3.1: "A driver MUST NOT create a
/// descriptor chain longer than the Queue Size of the device").
#[async_test]
async fn verify_indirect_chain_clamped_to_queue_size(driver: DefaultDriver) {
    let queue_size: u16 = 4;
    let test_mem = VirtioTestMemoryAccess::new();
    let mut guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, queue_size, true);
    let event = Event::new();
    let queue_event = PolledWait::new(&driver, event.clone()).unwrap();
    let mut queue = VirtioQueue::new(
        guest.queue_features(),
        guest.queue_params(0),
        guest.mem(),
        Interrupt::from_fn(|| {}),
        queue_event,
        None,
    )
    .unwrap();

    // Build an indirect descriptor pointing to an 8-entry table (> queue_size).
    let indirect_entries: u16 = 8;
    let next_descriptor = 0u16;
    let desc_base = guest.get_queue_descriptor(0, next_descriptor);
    // Set INDIRECT flag on the ring descriptor.
    test_mem.modify_memory_map(
        desc_base + 12,
        &u16::from(DescriptorFlags::new().with_indirect(true)).to_le_bytes(),
        false,
    );
    // Point addr/len to our indirect table in the backing buffer area.
    let buffer_addr = guest.get_queue_descriptor_backing_memory_address(0);
    test_mem.modify_memory_map(desc_base, &buffer_addr.to_le_bytes(), false);
    test_mem.modify_memory_map(
        desc_base + 8,
        &(indirect_entries as u32 * 16).to_le_bytes(),
        false,
    );
    // Fill out the indirect table: 8 linked entries.
    for i in 0..indirect_entries {
        let base = buffer_addr + 0x10 * i as u64;
        let indirect_buffer_addr = 0xffffffff00000000u64 + 0x1000 * i as u64;
        test_mem.modify_memory_map(base, &indirect_buffer_addr.to_le_bytes(), false);
        test_mem.modify_memory_map(base + 8, &0x1000u32.to_le_bytes(), false);
        let flags = if i < indirect_entries - 1 {
            u16::from(DescriptorFlags::new().with_next(true))
        } else {
            0
        };
        test_mem.modify_memory_map(base + 12, &flags.to_le_bytes(), false);
        let next = if i < indirect_entries - 1 { i + 1 } else { 0 };
        test_mem.modify_memory_map(base + 14, &next.to_le_bytes(), false);
    }
    guest.queue_available_desc(0, next_descriptor);

    let result = queue.try_next();
    assert!(
        result.is_err(),
        "Indirect chain exceeding queue_size must be rejected"
    );
}

/// An indirect table with exactly queue_size entries (last has no NEXT)
/// must succeed — this is the maximum valid indirect chain.
#[async_test]
async fn verify_indirect_chain_at_queue_size_succeeds(driver: DefaultDriver) {
    let queue_size: u16 = 4;
    let test_mem = VirtioTestMemoryAccess::new();
    let mut guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, queue_size, true);
    let (tx, mut rx) = mesh::mpsc_channel();
    let event = Event::new();
    let mut queues = guest.create_direct_queues(|i| {
        let tx = tx.clone();
        CreateDirectQueueParams {
            process_work: Box::new(
                move |queue: &mut VirtioQueue, work: VirtioQueueCallbackWork| {
                    assert_eq!(work.payload.len(), queue_size as usize);
                    queue.complete(work, 99);
                },
            ),
            notify: Interrupt::from_fn(move || {
                tx.send(i as usize);
            }),
            event: event.clone(),
        }
    });

    guest.add_indirect_linked_to_avail_queue(0, queue_size);
    event.signal();
    must_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;
    let (desc, len) = guest.get_next_completed(0).unwrap();
    assert_eq!(desc, 0u16);
    assert_eq!(len, 99);
    queues[0].stop().await;
}

/// A chain of normal descriptors followed by an indirect descriptor must work
/// (spec §2.7.5.3.2: "The device MUST handle the case of zero or more normal
/// chained descriptors followed by a single descriptor with
/// flags&VIRTQ_DESC_F_INDIRECT").
#[async_test]
async fn verify_normal_then_indirect_succeeds(driver: DefaultDriver) {
    let queue_size: u16 = 8;
    let test_mem = VirtioTestMemoryAccess::new();
    let mut guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, queue_size, true);
    let (tx, mut rx) = mesh::mpsc_channel();
    let event = Event::new();
    let mut queues = guest.create_direct_queues(|i| {
        let tx = tx.clone();
        CreateDirectQueueParams {
            process_work: Box::new(
                move |queue: &mut VirtioQueue, work: VirtioQueueCallbackWork| {
                    // 1 normal descriptor + 3 indirect entries = 4 payload entries.
                    // (The indirect head descriptor is not itself a payload entry.)
                    assert_eq!(work.payload.len(), 4);
                    queue.complete(work, 77);
                },
            ),
            notify: Interrupt::from_fn(move || {
                tx.send(i as usize);
            }),
            event: event.clone(),
        }
    });

    // Build: desc 0 (normal, NEXT→1) → desc 1 (INDIRECT, points to 3-entry table).
    // Reserve the two ring descriptors we're using manually.
    let d0 = guest.reserve_split_descriptor(0);
    let d1 = guest.reserve_split_descriptor(0);
    assert_eq!(d0, 0);
    assert_eq!(d1, 1);

    // Desc 0: normal descriptor with NEXT flag.
    let desc0_base = guest.get_queue_descriptor(0, 0);
    test_mem.modify_memory_map(
        desc0_base + 12,
        &u16::from(DescriptorFlags::new().with_next(true)).to_le_bytes(),
        false,
    );
    test_mem.modify_memory_map(desc0_base + 14, &1u16.to_le_bytes(), false);

    // Desc 1: INDIRECT descriptor pointing to a 3-entry indirect table.
    let desc1_base = guest.get_queue_descriptor(0, 1);
    let indirect_table_addr = 0xAABB00000000u64;
    let indirect_entries: u16 = 3;
    test_mem.modify_memory_map(desc1_base, &indirect_table_addr.to_le_bytes(), false);
    test_mem.modify_memory_map(
        desc1_base + 8,
        &(indirect_entries as u32 * 16).to_le_bytes(),
        false,
    );
    test_mem.modify_memory_map(
        desc1_base + 12,
        &u16::from(DescriptorFlags::new().with_indirect(true)).to_le_bytes(),
        false,
    );

    // Create the indirect table in memory at indirect_table_addr.
    for i in 0..indirect_entries {
        let entry_base = indirect_table_addr + 0x10 * i as u64;
        let entry_buf_addr = 0xDDEE00000000u64 + 0x1000 * i as u64;
        test_mem.modify_memory_map(entry_base, &entry_buf_addr.to_le_bytes(), false);
        test_mem.modify_memory_map(entry_base + 8, &0x1000u32.to_le_bytes(), false);
        let flags = if i < indirect_entries - 1 {
            u16::from(DescriptorFlags::new().with_next(true))
        } else {
            0
        };
        test_mem.modify_memory_map(entry_base + 12, &flags.to_le_bytes(), false);
        let next = if i < indirect_entries - 1 { i + 1 } else { 0 };
        test_mem.modify_memory_map(entry_base + 14, &next.to_le_bytes(), false);
    }

    guest.queue_available_desc(0, 0);
    event.signal();
    must_recv_in_timeout(&mut rx, Duration::from_millis(100)).await;
    let (desc, len) = guest.get_next_completed(0).unwrap();
    assert_eq!(desc, 0u16);
    assert_eq!(len, 77);
    queues[0].stop().await;
}

// ---------------------------------------------------------------------------
// Peek tests
// ---------------------------------------------------------------------------

async fn verify_peek_does_not_advance(mut guest: VirtioTestGuest) {
    let event = Event::new();
    let queue_event = PolledWait::new(&guest.driver(), event.clone()).unwrap();
    let notify = Interrupt::from_fn(|| {});
    let mut queue = VirtioQueue::new(
        guest.queue_features(),
        guest.queue_params(0),
        guest.mem(),
        notify,
        queue_event,
        None,
    )
    .expect("failed to create virtio queue");

    // Make one descriptor available.
    guest.add_to_avail_queue(0);
    event.signal();

    // Peek should return Some.
    let peeked = queue.try_peek().unwrap();
    assert!(peeked.is_some(), "expected a peeked descriptor");
    let peeked = peeked.unwrap();
    let first_payload_addr = peeked.payload()[0].address;

    // Drop without consuming.
    drop(peeked);

    // Peek again — should return the *same* descriptor since we didn't advance.
    let peeked2 = queue
        .try_peek()
        .unwrap()
        .expect("same descriptor available");
    assert_eq!(
        peeked2.payload()[0].address,
        first_payload_addr,
        "peeking again must return the same descriptor"
    );

    // The used ring should be empty — nothing was completed.
    assert!(
        guest.get_next_completed(0).is_none(),
        "no completions expected after drop"
    );

    // Consume the peeked work and complete it.
    let work = peeked2.consume();
    queue.complete(work, 42);

    // Now the used ring should have the completion.
    let (_, len) = guest.get_next_completed(0).expect("completion expected");
    assert_eq!(len, 42);

    // And a subsequent peek should return None (queue drained).
    assert!(
        queue.try_peek().unwrap().is_none(),
        "queue should be empty after consume"
    );
}

#[async_test]
async fn split_queue_state_initial_zero(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    let guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, 4, false);
    let event = Event::new();
    let queue_event = PolledWait::new(&driver, event.clone()).unwrap();
    let queue = VirtioQueue::new(
        guest.queue_features(),
        guest.queue_params(0),
        guest.mem(),
        Interrupt::from_fn(|| {}),
        queue_event,
        None,
    )
    .unwrap();
    let state = queue.queue_state();
    assert_eq!(
        state,
        QueueState {
            avail_index: 0,
            used_index: 0
        }
    );
}

#[async_test]
async fn verify_split_peek_does_not_advance(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    verify_peek_does_not_advance(VirtioTestGuest::new_split(&driver, &test_mem, 1, 2, true)).await;
}

#[async_test]
async fn verify_packed_peek_does_not_advance(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    verify_peek_does_not_advance(VirtioTestGuest::new_packed(&driver, &test_mem, 1, 2, true)).await;
}

async fn verify_peek_then_next(mut guest: VirtioTestGuest) {
    let event = Event::new();
    let queue_event = PolledWait::new(&guest.driver(), event.clone()).unwrap();
    let notify = Interrupt::from_fn(|| {});
    let mut queue = VirtioQueue::new(
        guest.queue_features(),
        guest.queue_params(0),
        guest.mem(),
        notify,
        queue_event,
        None,
    )
    .expect("failed to create virtio queue");

    // Make two descriptors available.
    guest.add_to_avail_queue(0);
    guest.add_to_avail_queue(0);
    event.signal();

    // Peek at the first — don't consume.
    {
        let peeked = queue.try_peek().unwrap().expect("first descriptor");
        // Drop it without consuming.
        drop(peeked);
    }

    // try_next should return the same first descriptor (peek didn't advance).
    let work = queue
        .try_next()
        .unwrap()
        .expect("first descriptor via next");
    let first_desc = work.descriptor_index();
    queue.complete(work, 10);

    // Now the next descriptor should be different.
    let work2 = queue.try_next().unwrap().expect("second descriptor");
    assert_ne!(
        work2.descriptor_index(),
        first_desc,
        "second descriptor should differ"
    );
    queue.complete(work2, 20);

    let (_, len) = guest.get_next_completed(0).expect("first completion");
    assert_eq!(len, 10);
    let (_, len) = guest.get_next_completed(0).expect("second completion");
    assert_eq!(len, 20);
    assert!(guest.get_next_completed(0).is_none());
}

#[async_test]
async fn verify_split_peek_then_next(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    verify_peek_then_next(VirtioTestGuest::new_split(&driver, &test_mem, 1, 2, true)).await;
}

#[async_test]
async fn verify_packed_peek_then_next(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    verify_peek_then_next(VirtioTestGuest::new_packed(&driver, &test_mem, 1, 2, true)).await;
}

/// Peek a linked buffer that spans multiple packed descriptors, then consume it.
/// Exercises the `descriptor_count` / `advance(count)` path for packed queues.
async fn verify_packed_peek_linked(mut guest: VirtioTestGuest) {
    let base_address = guest.get_queue_descriptor_backing_memory_address(0);
    let event = Event::new();
    let queue_event = PolledWait::new(&guest.driver(), event.clone()).unwrap();
    let notify = Interrupt::from_fn(|| {});
    let mut queue = VirtioQueue::new(
        guest.queue_features(),
        guest.queue_params(0),
        guest.mem(),
        notify,
        queue_event,
        None,
    )
    .expect("failed to create virtio queue");

    let desc_count = 3;
    guest.add_linked_to_avail_queue(0, desc_count);
    event.signal();

    // Peek should return a work item spanning all linked descriptors.
    let peeked = queue
        .try_peek()
        .unwrap()
        .expect("linked descriptor available");
    assert_eq!(
        peeked.payload().len(),
        desc_count as usize,
        "peek should return all linked descriptors"
    );
    for i in 0..desc_count as usize {
        assert_eq!(
            peeked.payload()[i].address,
            base_address + 0x1000 * i as u64
        );
        assert_eq!(peeked.payload()[i].length, 0x1000);
    }

    // Drop without consuming — descriptor should remain available.
    drop(peeked);

    // Peek again — same linked descriptor chain.
    let peeked2 = queue
        .try_peek()
        .unwrap()
        .expect("same linked descriptor still available");
    assert_eq!(peeked2.payload().len(), desc_count as usize);

    // Consume and complete.
    let work = peeked2.consume();
    assert_eq!(work.payload.len(), desc_count as usize);
    queue.complete(work, 99);

    let (_, len) = guest.get_next_completed(0).expect("completion expected");
    assert_eq!(len, 99);

    // Queue should be empty now.
    assert!(
        queue.try_peek().unwrap().is_none(),
        "queue should be empty after consuming linked descriptors"
    );
}

#[async_test]
async fn verify_packed_peek_linked_multi_descriptor(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    // Need enough queue size to hold the linked descriptors.
    verify_packed_peek_linked(VirtioTestGuest::new_packed(&driver, &test_mem, 1, 8, true)).await;
}

#[async_test]
async fn split_queue_state_advances_on_pop(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    let mut guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, 4, false);
    let event = Event::new();
    let queue_event = PolledWait::new(&driver, event.clone()).unwrap();
    let mut queue = VirtioQueue::new(
        guest.queue_features(),
        guest.queue_params(0),
        guest.mem(),
        Interrupt::from_fn(|| {}),
        queue_event,
        None,
    )
    .unwrap();

    guest.queue_available_desc(0, 0);
    let work = queue.try_next().unwrap().unwrap();
    let state = queue.queue_state();
    assert_eq!(state.avail_index, 1);
    // Complete the descriptor → used_index advances
    queue.complete(work, 0);
    let state = queue.queue_state();
    assert_eq!(state.used_index, 1);
}

#[async_test]
async fn split_queue_new_with_state_roundtrip(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    let guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, 4, false);
    let event = Event::new();
    let queue_event = PolledWait::new(&driver, event.clone()).unwrap();
    let initial = QueueState {
        avail_index: 5,
        used_index: 3,
    };
    let queue = VirtioQueue::new(
        guest.queue_features(),
        guest.queue_params(0),
        guest.mem(),
        Interrupt::from_fn(|| {}),
        queue_event,
        Some(initial),
    )
    .unwrap();
    assert_eq!(queue.queue_state(), initial);
}

#[async_test]
async fn packed_queue_state_initial(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    let guest = VirtioTestGuest::new_packed(&driver, &test_mem, 1, 4, false);
    let event = Event::new();
    let queue_event = PolledWait::new(&driver, event.clone()).unwrap();
    let queue = VirtioQueue::new(
        guest.queue_features(),
        guest.queue_params(0),
        guest.mem(),
        Interrupt::from_fn(|| {}),
        queue_event,
        None,
    )
    .unwrap();
    let state = queue.queue_state();
    // Initial packed state: index=0, wrap=true → avail = 0 | (1 << 15) = 0x8000
    assert_eq!(state.avail_index, 0x8000);
    assert_eq!(state.used_index, 0x8000);
}

#[async_test]
async fn packed_queue_new_with_state_roundtrip(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    let guest = VirtioTestGuest::new_packed(&driver, &test_mem, 1, 4, false);
    let event = Event::new();
    let queue_event = PolledWait::new(&driver, event.clone()).unwrap();
    // index=3, wrap=true → 3 | 0x8000 = 0x8003
    let initial = QueueState {
        avail_index: 0x8003,
        used_index: 0x8001,
    };
    let queue = VirtioQueue::new(
        guest.queue_features(),
        guest.queue_params(0),
        guest.mem(),
        Interrupt::from_fn(|| {}),
        queue_event,
        Some(initial),
    )
    .unwrap();
    assert_eq!(queue.queue_state(), initial);
}

// ---------------------------------------------------------------------------
// Queue failure / disable tests
// ---------------------------------------------------------------------------

/// A device where `start_queue` succeeds for indices < `fail_at` and
/// fails at `fail_at`. Tracks start/stop calls for assertions.
#[derive(InspectMut)]
#[inspect(skip)]
struct PartialFailTestDevice {
    traits: DeviceTraits,
    fail_at: u16,
    started: Vec<u16>,
    stopped: Vec<u16>,
    reset_count: usize,
}

impl PartialFailTestDevice {
    fn new(max_queues: u16, fail_at: u16) -> Self {
        Self {
            traits: DeviceTraits {
                device_id: VirtioDeviceType::CONSOLE,
                device_features: VirtioDeviceFeatures::new()
                    .with_bank(0, 2 | VIRTIO_F_RING_INDIRECT_DESC | VIRTIO_F_RING_EVENT_IDX),
                max_queues,
                device_register_length: 0,
                ..Default::default()
            },
            fail_at,
            started: Vec::new(),
            stopped: Vec::new(),
            reset_count: 0,
        }
    }
}

impl VirtioDevice for PartialFailTestDevice {
    fn traits(&self) -> DeviceTraits {
        self.traits.clone()
    }
    async fn read_registers_u32(&mut self, _offset: u16) -> u32 {
        0
    }
    async fn write_registers_u32(&mut self, _offset: u16, _val: u32) {}
    async fn start_queue(
        &mut self,
        idx: u16,
        _resources: QueueResources,
        _features: &VirtioDeviceFeatures,
        _initial_state: Option<QueueState>,
    ) -> anyhow::Result<()> {
        if idx == self.fail_at {
            anyhow::bail!("intentional failure on queue {idx}");
        }
        self.started.push(idx);
        Ok(())
    }
    async fn stop_queue(&mut self, idx: u16) -> Option<QueueState> {
        self.stopped.push(idx);
        None
    }
    async fn reset(&mut self) {
        self.reset_count += 1;
    }
}

/// Transport-agnostic test harness. Wraps either MMIO or PCI device behind
/// a uniform interface so tests can be written once.
trait TestTransport {
    /// Write DRIVER_OK after setup (queues already configured + enabled).
    fn write_driver_ok(&mut self);
    /// Write STATUS=0 (guest reset).
    fn write_status_zero(&mut self);
    /// Read the device status register.
    fn read_status(&mut self) -> u32;
    /// Read the config generation counter.
    fn read_config_generation(&mut self) -> u32;
    /// Drive poll_device once with a noop waker.
    fn poll_once(&mut self);
    /// Drive ChangeDeviceState::stop().
    fn stop(&mut self) -> impl Future<Output = ()>;
    /// Drive ChangeDeviceState::start().
    fn start(&mut self);
}

/// Yield to the executor and poll the transport once. This ensures the device
/// task has a chance to process any commands and the transport picks up the
/// result.
async fn yield_and_poll(transport: &mut impl TestTransport) {
    yield_now().await;
    transport.poll_once();
}

struct MmioTestTransport {
    dev: VirtioMmioDevice,
}

impl MmioTestTransport {
    fn new(device: Box<dyn DynVirtioDevice>, driver: &DefaultDriver, num_queues: u16) -> Self {
        let test_mem = VirtioTestMemoryAccess::new();
        let doorbell_registration: Arc<dyn DoorbellRegistration> = test_mem;
        let interrupt = LineInterrupt::detached();
        let mut dev = VirtioMmioDevice::new(
            device,
            driver,
            GuestMemory::empty(),
            interrupt,
            Some(doorbell_registration),
            0,
            0x1000,
        )
        .unwrap();

        // Drive through ACKNOWLEDGE -> DRIVER -> features -> FEATURES_OK
        dev.write_u32(112, VIRTIO_ACKNOWLEDGE);
        dev.write_u32(112, VIRTIO_DRIVER);
        dev.write_u32(36, 0);
        dev.write_u32(32, 2);
        dev.write_u32(36, 1);
        dev.write_u32(32, VIRTIO_F_VERSION_1);
        dev.write_u32(112, VIRTIO_FEATURES_OK);

        // Set up and enable all queues
        for i in 0..num_queues {
            dev.write_u32(48, i as u32); // queue select
            dev.write_u32(56, 16); // queue size
            dev.write_u32(68, 1); // queue enable
        }

        Self { dev }
    }
}

impl TestTransport for MmioTestTransport {
    fn write_driver_ok(&mut self) {
        self.dev.write_u32(112, VIRTIO_DRIVER_OK);
    }
    fn write_status_zero(&mut self) {
        self.dev.write_u32(112, 0);
    }
    fn read_status(&mut self) -> u32 {
        self.dev.read_u32(112)
    }
    fn read_config_generation(&mut self) -> u32 {
        self.dev.read_u32(0xfc)
    }
    fn poll_once(&mut self) {
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        self.dev.poll_device(&mut cx);
    }
    async fn stop(&mut self) {
        ChangeDeviceState::stop(&mut self.dev).await;
    }
    fn start(&mut self) {
        ChangeDeviceState::start(&mut self.dev);
    }
}

struct PciTestTransport {
    dev: VirtioPciDevice,
}

impl PciTestTransport {
    fn new(device: Box<dyn DynVirtioDevice>, driver: &DefaultDriver, num_queues: u16) -> Self {
        let test_mem = VirtioTestMemoryAccess::new();
        let doorbell_registration: Arc<dyn DoorbellRegistration> = test_mem;
        let msi_conn = MsiConnection::new(AssignedBusRange::new(), 0);

        let mut dev = VirtioPciDevice::new(
            device,
            driver,
            GuestMemory::empty(),
            PciInterruptModel::Msix(msi_conn.target()),
            Some(doorbell_registration),
            &mut ExternallyManagedMmioIntercepts,
            None,
        )
        .unwrap();

        let bar_address: u64 = 0x10000000000;
        dev.pci_cfg_write(0x14, (bar_address >> 32) as u32).unwrap();
        dev.pci_cfg_write(0x10, bar_address as u32).unwrap();

        let bar_address2: u64 = 0x20000000000;
        dev.pci_cfg_write(0x1c, (bar_address2 >> 32) as u32)
            .unwrap();
        dev.pci_cfg_write(0x18, bar_address2 as u32).unwrap();

        dev.pci_cfg_write(
            0x4,
            cfg_space::Command::new()
                .with_mmio_enabled(true)
                .into_bits() as u32,
        )
        .unwrap();

        // Status: ACKNOWLEDGE -> DRIVER
        let mut buf = [0u8; 1];
        buf[0] = VIRTIO_ACKNOWLEDGE as u8;
        dev.mmio_write(bar_address + 20, &buf).unwrap();
        buf[0] = VIRTIO_DRIVER as u8;
        dev.mmio_write(bar_address + 20, &buf).unwrap();

        // Features
        dev.mmio_write(bar_address + 8, &0u32.to_le_bytes())
            .unwrap();
        dev.mmio_write(bar_address + 12, &2u32.to_le_bytes())
            .unwrap();
        dev.mmio_write(bar_address + 8, &1u32.to_le_bytes())
            .unwrap();
        dev.mmio_write(bar_address + 12, &VIRTIO_F_VERSION_1.to_le_bytes())
            .unwrap();

        buf[0] = VIRTIO_FEATURES_OK as u8;
        dev.mmio_write(bar_address + 20, &buf).unwrap();

        // MSI config vector
        dev.mmio_write(bar_address2, &0u64.to_le_bytes()).unwrap();
        dev.mmio_write(bar_address2 + 8, &0u32.to_le_bytes())
            .unwrap();
        dev.mmio_write(bar_address2 + 12, &0u32.to_le_bytes())
            .unwrap();

        // Set up queues
        for i in 0..num_queues {
            dev.mmio_write(bar_address + 22, &i.to_le_bytes()).unwrap();
            dev.mmio_write(bar_address + 24, &16u16.to_le_bytes())
                .unwrap();
            let msix_vector = i + 1;
            let msix_addr = bar_address2 + 0x10 * msix_vector as u64;
            dev.mmio_write(msix_addr, &(msix_vector as u64).to_le_bytes())
                .unwrap();
            dev.mmio_write(msix_addr + 8, &0u32.to_le_bytes()).unwrap();
            dev.mmio_write(msix_addr + 12, &0u32.to_le_bytes()).unwrap();
            dev.mmio_write(bar_address + 26, &msix_vector.to_le_bytes())
                .unwrap();
            dev.mmio_write(bar_address + 28, &1u16.to_le_bytes())
                .unwrap();
        }
        dev.pci_cfg_write(0x40, 0x80000000).unwrap();

        Self { dev }
    }
}

impl TestTransport for PciTestTransport {
    fn write_driver_ok(&mut self) {
        // Use the test helper to bypass MmioIntercept stall/deferred logic.
        let current = self.dev.read_u32(20);
        self.dev.write_u32(20, (current & !0xff) | VIRTIO_DRIVER_OK);
    }
    fn write_status_zero(&mut self) {
        let current = self.dev.read_u32(20);
        self.dev.write_u32(20, current & !0xff);
    }
    fn read_status(&mut self) -> u32 {
        self.dev.read_u32(20) & 0xff
    }
    fn read_config_generation(&mut self) -> u32 {
        (self.dev.read_u32(20) >> 8) & 0xff
    }
    fn poll_once(&mut self) {
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        self.dev.poll_device(&mut cx);
    }
    async fn stop(&mut self) {
        ChangeDeviceState::stop(&mut self.dev).await;
    }
    fn start(&mut self) {
        ChangeDeviceState::start(&mut self.dev);
    }
}

// -- Shared test logic, parameterized over transport --

async fn verify_partial_failure_enters_disabling(transport: &mut impl TestTransport) {
    // Attempt DRIVER_OK — queue 1 will fail, queue 0 succeeded.
    transport.write_driver_ok();

    // Yield to let device task process the Enable command.
    yield_and_poll(transport).await;

    // DRIVER_OK must NOT be set.
    let status = transport.read_status();
    assert_eq!(
        status & VIRTIO_DRIVER_OK,
        0,
        "DRIVER_OK must not be set when a queue fails to start"
    );

    // After poll_device completes the enable failure, status should be fully reset.
    let status = transport.read_status();
    assert_eq!(status, 0, "status should be reset after disable completes");
}

async fn verify_stop_completes_pending_disable(transport: &mut impl TestTransport) {
    // Trigger failure → enters disabling state.
    transport.write_driver_ok();
    yield_and_poll(transport).await;
    assert_eq!(transport.read_status() & VIRTIO_DRIVER_OK, 0);

    // Call stop() — should be a no-op since enable failure already cleaned up.
    transport.stop().await;

    // Status should be fully reset.
    let status = transport.read_status();
    assert_eq!(status, 0, "stop() should complete the pending disable");
}

// -- MMIO tests --

#[async_test]
async fn partial_queue_failure_enters_disabling_mmio(_driver: DefaultDriver) {
    let mut transport =
        MmioTestTransport::new(Box::new(PartialFailTestDevice::new(2, 1)), &_driver, 2);
    verify_partial_failure_enters_disabling(&mut transport).await;
}

#[async_test]
async fn stop_completes_pending_disable_mmio(_driver: DefaultDriver) {
    let mut transport =
        MmioTestTransport::new(Box::new(PartialFailTestDevice::new(2, 1)), &_driver, 2);
    verify_stop_completes_pending_disable(&mut transport).await;
}

// -- PCI tests --

#[async_test]
async fn partial_queue_failure_enters_disabling_pci(_driver: DefaultDriver) {
    let mut transport =
        PciTestTransport::new(Box::new(PartialFailTestDevice::new(2, 1)), &_driver, 2);
    verify_partial_failure_enters_disabling(&mut transport).await;
}

#[async_test]
async fn stop_completes_pending_disable_pci(_driver: DefaultDriver) {
    let mut transport =
        PciTestTransport::new(Box::new(PartialFailTestDevice::new(2, 1)), &_driver, 2);
    verify_stop_completes_pending_disable(&mut transport).await;
}

async fn verify_reset_during_enable_disables_queues(transport: &mut impl TestTransport) {
    // Write DRIVER_OK — starts an async Enable.
    transport.write_driver_ok();

    // Yield so the device task processes the Enable (which succeeds).
    yield_now().await;

    // Poll once — enable completes, DRIVER_OK is set.
    transport.poll_once();

    assert_ne!(
        transport.read_status() & VIRTIO_DRIVER_OK,
        0,
        "DRIVER_OK must be set after successful enable"
    );

    // Now write STATUS=0 to trigger a disable.
    transport.write_status_zero();

    // Yield so the device task processes the Disable (stops queues).
    yield_now().await;

    // Poll to observe DisableComplete.
    transport.poll_once();

    // Status must be fully reset.
    assert_eq!(
        transport.read_status(),
        0,
        "status must be 0 after enable-then-reset completes"
    );
}

#[async_test]
async fn reset_during_enable_disables_queues_mmio(_driver: DefaultDriver) {
    // Use PartialFailTestDevice with fail_at > max_queues so all queues succeed.
    let mut transport =
        MmioTestTransport::new(Box::new(PartialFailTestDevice::new(1, 99)), &_driver, 1);
    verify_reset_during_enable_disables_queues(&mut transport).await;
}

#[async_test]
async fn reset_during_enable_disables_queues_pci(_driver: DefaultDriver) {
    let mut transport =
        PciTestTransport::new(Box::new(PartialFailTestDevice::new(1, 99)), &_driver, 1);
    verify_reset_during_enable_disables_queues(&mut transport).await;
}

/// Verify that resetting a PCI device using IntX interrupts deasserts the IRQ line.
///
/// If poll_disable_all only clears interrupt_status without calling
/// line.set_level(false), the IntX line stays asserted after reset.
#[async_test]
async fn pci_intx_line_deasserted_on_reset(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    let doorbell_registration: Arc<dyn DoorbellRegistration> = test_mem.clone();
    let mem = GuestMemory::new("test", test_mem.clone());
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));

    let intc = TestLineInterruptTarget::new_arc();
    let vector = 0;
    let line = LineInterrupt::new_with_target("pci-intx-test", intc.clone(), vector);

    let mut guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, 2, false);

    let mut dev = VirtioPciDevice::new(
        Box::new(TestDevice::new(
            &driver_source,
            DeviceTraits {
                device_id: VirtioDeviceType::CONSOLE,
                device_features: VirtioDeviceFeatures::new()
                    .with_bank(0, 2 | VIRTIO_F_RING_INDIRECT_DESC | VIRTIO_F_RING_EVENT_IDX),
                max_queues: 1,
                device_register_length: 12,
                ..Default::default()
            },
            Some(Arc::new(
                |_i, queue: &mut VirtioQueue, work: VirtioQueueCallbackWork| {
                    queue.complete(work, 42);
                },
            )),
        )),
        &driver,
        mem,
        PciInterruptModel::IntX(pci_core::PciInterruptPin::IntA, line),
        Some(doorbell_registration),
        &mut ExternallyManagedMmioIntercepts,
        None,
    )
    .unwrap();

    let bar_address: u64 = 0x10000000000;
    dev.pci_cfg_write(0x14, (bar_address >> 32) as u32).unwrap();
    dev.pci_cfg_write(0x10, bar_address as u32).unwrap();
    dev.pci_cfg_write(
        0x4,
        cfg_space::Command::new()
            .with_mmio_enabled(true)
            .into_bits() as u32,
    )
    .unwrap();

    // ACKNOWLEDGE -> DRIVER
    dev.mmio_write(bar_address + 20, &[VIRTIO_ACKNOWLEDGE as u8])
        .unwrap();
    dev.mmio_write(bar_address + 20, &[VIRTIO_DRIVER as u8])
        .unwrap();

    // Accept features
    dev.mmio_write(bar_address + 8, &0u32.to_le_bytes())
        .unwrap();
    dev.mmio_write(bar_address + 12, &2u32.to_le_bytes())
        .unwrap();
    dev.mmio_write(bar_address + 8, &1u32.to_le_bytes())
        .unwrap();
    dev.mmio_write(bar_address + 12, &VIRTIO_F_VERSION_1.to_le_bytes())
        .unwrap();
    dev.mmio_write(bar_address + 20, &[VIRTIO_FEATURES_OK as u8])
        .unwrap();

    // Set up queue 0 with addresses from test guest memory layout.
    // queue_select = 0 (write to high half of DEVICE_STATUS register)
    dev.mmio_write(bar_address + 22, &0u16.to_le_bytes())
        .unwrap();
    // queue_size = 2 (low half of QUEUE_SIZE register)
    dev.mmio_write(bar_address + 24, &2u16.to_le_bytes())
        .unwrap();
    // queue descriptor address
    let desc_addr = guest.get_queue_descriptor_base_address(0);
    dev.mmio_write(bar_address + 32, &(desc_addr as u32).to_le_bytes())
        .unwrap();
    dev.mmio_write(bar_address + 36, &((desc_addr >> 32) as u32).to_le_bytes())
        .unwrap();
    // queue available address
    let avail_addr = guest.get_queue_available_base_address(0);
    dev.mmio_write(bar_address + 40, &(avail_addr as u32).to_le_bytes())
        .unwrap();
    dev.mmio_write(bar_address + 44, &((avail_addr >> 32) as u32).to_le_bytes())
        .unwrap();
    // queue used address
    let used_addr = guest.get_queue_used_base_address(0);
    dev.mmio_write(bar_address + 48, &(used_addr as u32).to_le_bytes())
        .unwrap();
    dev.mmio_write(bar_address + 52, &((used_addr >> 32) as u32).to_le_bytes())
        .unwrap();
    // enable queue
    dev.mmio_write(bar_address + 28, &1u16.to_le_bytes())
        .unwrap();

    // DRIVER_OK — starts the queue worker (use write_u32 to bypass deferred IO)
    let current = dev.read_u32(20);
    dev.write_u32(20, (current & !0xff) | VIRTIO_DRIVER_OK);

    yield_and_poll_device(&mut dev).await;

    // Add a buffer to the avail ring and notify the device to process it.
    guest.add_to_avail_queue(0);
    dev.mmio_write(bar_address + 0x38, &0u32.to_le_bytes())
        .unwrap();

    // Wait for the queue worker to process the buffer and fire the IntX interrupt.
    poll_fn(|cx| intc.poll_high(cx, vector)).await;

    // Reset the device (write 0 to status) — use write_u32 to bypass deferred IO.
    let current = dev.read_u32(20);
    dev.write_u32(20, current & !0xff);

    // Poll until the async disable completes and status resets to 0.
    let mut timer = PolledTimer::new(&driver);
    loop {
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        dev.poll_device(&mut cx);
        if dev.read_u32(20) & 0xff == 0 {
            break;
        }
        timer.sleep(Duration::from_millis(10)).await;
    }

    // After the fix, poll_disable_all deasserts the IntX line.
    assert!(
        !intc.is_high(vector),
        "IntX line must be low after device reset"
    );
}

// ==================== Save/Restore Tests ====================

#[async_test]
async fn pci_save_restore_round_trip(driver: DefaultDriver) {
    use vmcore::device_state::ChangeDeviceState;
    use vmcore::save_restore::SaveRestore;

    let test_mem = VirtioTestMemoryAccess::new();

    let guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, 4, true);

    let mut dev = VirtioPciTestDevice::new(&driver, 1, &test_mem, None);
    guest
        .setup_pci_device(&mut dev, guest.queue_features())
        .await;

    // Stop the device (save path).
    dev.pci_device.stop().await;

    // Save state.
    let saved = dev.pci_device.save().expect("save should succeed");

    // Verify saved state has expected values.
    assert_eq!(
        saved.common.device_status,
        u8::from(
            VirtioDeviceStatus::new()
                .with_acknowledge(true)
                .with_driver(true)
                .with_driver_ok(true)
                .with_features_ok(true)
        )
    );
    assert_eq!(saved.queues.len(), 1);
    assert!(saved.queues[0].common.enable);

    // Create a new device and restore into it.
    let mut dev2 = VirtioPciTestDevice::new(&driver, 1, &test_mem, None);
    dev2.pci_device
        .restore(saved)
        .expect("restore should succeed");

    // Verify stop is a no-op — restore must not start queues.
    dev2.pci_device.stop().await;
}

#[async_test]
async fn mmio_save_restore_round_trip(driver: DefaultDriver) {
    use crate::spec::mmio::VirtioMmioRegister;
    use vmcore::device_state::ChangeDeviceState;
    use vmcore::save_restore::SaveRestore;

    let test_mem = VirtioTestMemoryAccess::new();
    let doorbell_registration: Arc<dyn DoorbellRegistration> = test_mem.clone();
    let mem = GuestMemory::new("test", test_mem.clone());
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));

    let guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, 4, true);

    let interrupt = LineInterrupt::detached();
    let mut dev = VirtioMmioDevice::new(
        Box::new(TestDevice::new(
            &driver_source,
            DeviceTraits {
                device_id: VirtioDeviceType::CONSOLE,
                device_features: VirtioDeviceFeatures::new()
                    .with_bank(0, 2 | VIRTIO_F_RING_INDIRECT_DESC | VIRTIO_F_RING_EVENT_IDX),
                max_queues: 1,
                device_register_length: 0,
                ..Default::default()
            },
            None,
        )),
        &driver_source.simple(),
        mem.clone(),
        interrupt,
        Some(doorbell_registration.clone()),
        0,
        1,
    )
    .unwrap();

    guest
        .setup_chipset_device(&mut dev, guest.queue_features())
        .await;

    // Stop the device (save path).
    dev.stop().await;

    // Save state.
    let saved = dev.save().expect("save should succeed");
    assert_eq!(saved.queues.len(), 1);
    assert!(saved.queues[0].common.enable);

    // Create a new device and restore into it.
    let interrupt2 = LineInterrupt::detached();
    let mut dev2 = VirtioMmioDevice::new(
        Box::new(TestDevice::new(
            &driver_source,
            DeviceTraits {
                device_id: VirtioDeviceType::CONSOLE,
                device_features: VirtioDeviceFeatures::new()
                    .with_bank(0, 2 | VIRTIO_F_RING_INDIRECT_DESC | VIRTIO_F_RING_EVENT_IDX),
                max_queues: 1,
                device_register_length: 0,
                ..Default::default()
            },
            None,
        )),
        &driver_source.simple(),
        mem.clone(),
        interrupt2,
        Some(doorbell_registration),
        0,
        1,
    )
    .unwrap();

    dev2.restore(saved).expect("restore should succeed");
    // Verify device is active after restore — read STATUS register.
    assert_ne!(
        dev2.read_u32(VirtioMmioRegister::STATUS.0 as u64) & VIRTIO_DRIVER_OK,
        0
    );

    // Stop and clean up.
    dev2.stop().await;
}

#[async_test]
async fn pci_save_restore_incompatible_features(driver: DefaultDriver) {
    use vmcore::device_state::ChangeDeviceState;
    use vmcore::save_restore::SaveRestore;

    let test_mem = VirtioTestMemoryAccess::new();
    let mem = GuestMemory::new("test", test_mem.clone());
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));

    let guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, 4, true);

    let mut dev = VirtioPciTestDevice::new(&driver, 1, &test_mem, None);
    // Negotiate features including the device-specific bit (bank 0, bit 1).
    let mut driver_features = guest.queue_features();
    driver_features.set_bank(0, driver_features.bank(0) | 2);
    guest.setup_pci_device(&mut dev, driver_features).await;

    dev.pci_device.stop().await;
    let saved = dev.pci_device.save().expect("save should succeed");
    // Confirm saved state includes the device-specific feature bit.
    assert_ne!(saved.common.driver_feature_banks[0] & 2, 0);

    // Create a new device that does NOT support that device-specific feature.
    let msi_conn = MsiConnection::new(AssignedBusRange::new(), 0);
    let mut dev2 = VirtioPciDevice::new(
        Box::new(TestDevice::new(
            &driver_source,
            DeviceTraits {
                device_id: VirtioDeviceType::CONSOLE,
                device_features: VirtioDeviceFeatures::new()
                    .with_bank(0, VIRTIO_F_RING_INDIRECT_DESC | VIRTIO_F_RING_EVENT_IDX), // no device-specific features
                max_queues: 1,
                device_register_length: 12,
                ..Default::default()
            },
            None,
        )),
        &driver,
        mem,
        PciInterruptModel::Msix(msi_conn.target()),
        None,
        &mut ExternallyManagedMmioIntercepts,
        None,
    )
    .unwrap();

    let result = dev2.restore(saved);
    assert!(
        result.is_err(),
        "restore should fail with incompatible features"
    );
}

#[async_test]
async fn pci_save_not_supported_device(_driver: DefaultDriver) {
    use vmcore::save_restore::SaveRestore;

    let msi_conn = MsiConnection::new(AssignedBusRange::new(), 0);

    // FailingTestDevice does not override supports_save_restore (default false).
    let mut dev = VirtioPciDevice::new(
        Box::new(FailingTestDevice {
            traits: DeviceTraits {
                device_id: VirtioDeviceType::CONSOLE,
                device_features: VirtioDeviceFeatures::new(),
                max_queues: 1,
                device_register_length: 0,
                ..Default::default()
            },
        }),
        &_driver,
        GuestMemory::empty(),
        PciInterruptModel::Msix(msi_conn.target()),
        None,
        &mut ExternallyManagedMmioIntercepts,
        None,
    )
    .unwrap();

    let result = dev.save();
    assert!(result.is_err(), "save should fail for unsupported device");
}

#[async_test]
async fn mmio_save_not_supported_device(_driver: DefaultDriver) {
    use vmcore::save_restore::SaveRestore;

    let interrupt = LineInterrupt::detached();

    let mut dev = VirtioMmioDevice::new(
        Box::new(FailingTestDevice {
            traits: DeviceTraits {
                device_id: VirtioDeviceType::CONSOLE,
                device_features: VirtioDeviceFeatures::new(),
                max_queues: 1,
                device_register_length: 0,
                ..Default::default()
            },
        }),
        &_driver,
        GuestMemory::empty(),
        interrupt,
        None,
        0,
        1,
    )
    .unwrap();

    let result = dev.save();
    assert!(result.is_err(), "save should fail for unsupported device");
}

#[async_test]
async fn pci_restore_reinstalls_doorbells(driver: DefaultDriver) {
    use vmcore::save_restore::SaveRestore;

    let test_mem = VirtioTestMemoryAccess::new();

    let guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, 4, true);

    let mut dev = VirtioPciTestDevice::new(&driver, 1, &test_mem, None);
    guest
        .setup_pci_device(&mut dev, guest.queue_features())
        .await;

    // After setup, doorbells should be registered.
    let doorbells_after_setup = test_mem.doorbell_count.load(Ordering::Relaxed);
    assert!(
        doorbells_after_setup > 0,
        "doorbells should be registered after setup"
    );

    // Stop, save, restore into a new device.
    dev.pci_device.stop().await;
    let saved = dev.pci_device.save().expect("save should succeed");

    let mut dev2 = VirtioPciTestDevice::new(&driver, 1, &test_mem, None);
    // Configure BARs on the target device so doorbells can be registered.
    let bar_address1: u64 = 0x10000000000;
    dev2.pci_device
        .pci_cfg_write(0x14, (bar_address1 >> 32) as u32)
        .unwrap();
    dev2.pci_device
        .pci_cfg_write(0x10, bar_address1 as u32)
        .unwrap();
    dev2.pci_device
        .pci_cfg_write(
            0x4,
            cfg_space::Command::new()
                .with_mmio_enabled(true)
                .into_bits() as u32,
        )
        .unwrap();
    // Reset counter to isolate restore behavior.
    test_mem.doorbell_count.store(0, Ordering::Relaxed);
    dev2.pci_device
        .restore(saved)
        .expect("restore should succeed");

    // Doorbells must be reinstalled during restore.
    let doorbells_after_restore = test_mem.doorbell_count.load(Ordering::Relaxed);
    assert!(
        doorbells_after_restore > 0,
        "doorbells should be reinstalled after restore, got {doorbells_after_restore}"
    );

    dev2.pci_device.stop().await;
}

#[async_test]
async fn mmio_restore_reinstalls_doorbells(driver: DefaultDriver) {
    use vmcore::save_restore::SaveRestore;

    let test_mem = VirtioTestMemoryAccess::new();
    let doorbell_registration: Arc<dyn DoorbellRegistration> = test_mem.clone();
    let mem = GuestMemory::new("test", test_mem.clone());
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));

    let guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, 4, true);

    let interrupt = LineInterrupt::detached();
    let mut dev = VirtioMmioDevice::new(
        Box::new(TestDevice::new(
            &driver_source,
            DeviceTraits {
                device_id: VirtioDeviceType::CONSOLE,
                device_features: VirtioDeviceFeatures::new()
                    .with_bank(0, 2 | VIRTIO_F_RING_INDIRECT_DESC | VIRTIO_F_RING_EVENT_IDX),
                max_queues: 1,
                device_register_length: 0,
                ..Default::default()
            },
            None,
        )),
        &driver,
        mem.clone(),
        interrupt,
        Some(doorbell_registration.clone()),
        0,
        1,
    )
    .unwrap();

    guest
        .setup_chipset_device(&mut dev, guest.queue_features())
        .await;

    // After setup, doorbells should be registered.
    let doorbells_after_setup = test_mem.doorbell_count.load(Ordering::Relaxed);
    assert!(
        doorbells_after_setup > 0,
        "doorbells should be registered after setup"
    );

    // Stop, save, restore into a new device.
    dev.stop().await;
    let saved = dev.save().expect("save should succeed");

    let interrupt2 = LineInterrupt::detached();
    let mut dev2 = VirtioMmioDevice::new(
        Box::new(TestDevice::new(
            &driver_source,
            DeviceTraits {
                device_id: VirtioDeviceType::CONSOLE,
                device_features: VirtioDeviceFeatures::new()
                    .with_bank(0, 2 | VIRTIO_F_RING_INDIRECT_DESC | VIRTIO_F_RING_EVENT_IDX),
                max_queues: 1,
                device_register_length: 0,
                ..Default::default()
            },
            None,
        )),
        &driver,
        mem.clone(),
        interrupt2,
        Some(doorbell_registration),
        0,
        1,
    )
    .unwrap();

    // Reset counter to isolate restore behavior.
    test_mem.doorbell_count.store(0, Ordering::Relaxed);
    dev2.restore(saved).expect("restore should succeed");

    // Doorbells must be reinstalled during restore.
    let doorbells_after_restore = test_mem.doorbell_count.load(Ordering::Relaxed);
    assert!(
        doorbells_after_restore > 0,
        "doorbells should be reinstalled after restore, got {doorbells_after_restore}"
    );

    dev2.stop().await;
}

// -- Tests for drain / stop / reset side-effects --

/// Verify that stop() during an in-flight enable applies the enable result
/// so that start() correctly re-enables queues afterward. Without applying
/// the EnableComplete side-effects in stop(), device_status would lack
/// DRIVER_OK and start() would skip queue re-activation.
async fn verify_stop_during_enable_preserves_driver_ok(transport: &mut impl TestTransport) {
    // Write DRIVER_OK — starts an async Enable.
    transport.write_driver_ok();

    // Don't yield — the enable is still in flight. Call stop() which
    // must drain the in-flight enable and apply the EnableComplete result.
    transport.stop().await;

    // device_status must have DRIVER_OK set now.
    assert_ne!(
        transport.read_status() & VIRTIO_DRIVER_OK,
        0,
        "stop() must apply EnableComplete so DRIVER_OK is set"
    );

    // start() should re-enable queues (only happens when driver_ok is set).
    transport.start();
    yield_now().await;
}

#[async_test]
async fn stop_during_enable_preserves_driver_ok_mmio(_driver: DefaultDriver) {
    let mut transport =
        MmioTestTransport::new(Box::new(PartialFailTestDevice::new(1, 99)), &_driver, 1);
    verify_stop_during_enable_preserves_driver_ok(&mut transport).await;
}

#[async_test]
async fn stop_during_enable_preserves_driver_ok_pci(_driver: DefaultDriver) {
    let mut transport =
        PciTestTransport::new(Box::new(PartialFailTestDevice::new(1, 99)), &_driver, 1);
    verify_stop_during_enable_preserves_driver_ok(&mut transport).await;
}

/// Verify that stop() during an in-flight disable drains the disable
/// and fully resets status, including config_generation.
async fn verify_stop_drains_in_flight_disable(transport: &mut impl TestTransport) {
    // Write DRIVER_OK — starts an async Enable.
    transport.write_driver_ok();

    // Complete the enable.
    yield_now().await;
    transport.poll_once();

    // Write STATUS=0 — starts an async Disable.
    transport.write_status_zero();

    // stop() must drain the in-flight disable and apply
    // DisableComplete which calls reset_status().
    transport.stop().await;

    assert_eq!(
        transport.read_status(),
        0,
        "status must be fully reset after stop() drains an in-flight disable"
    );
    assert_eq!(
        transport.read_config_generation(),
        0,
        "config_generation must be 0 after stop() drains an in-flight disable"
    );
}

#[async_test]
async fn stop_drains_pending_reset_mmio(_driver: DefaultDriver) {
    let mut transport =
        MmioTestTransport::new(Box::new(PartialFailTestDevice::new(1, 99)), &_driver, 1);
    verify_stop_drains_in_flight_disable(&mut transport).await;
}

#[async_test]
async fn stop_drains_pending_reset_pci(_driver: DefaultDriver) {
    let mut transport =
        PciTestTransport::new(Box::new(PartialFailTestDevice::new(1, 99)), &_driver, 1);
    verify_stop_drains_in_flight_disable(&mut transport).await;
}

/// Verify that stop() during an in-flight failed enable resets
/// config_generation (the full reset_status path).
async fn verify_stop_during_failed_enable_resets_config(transport: &mut impl TestTransport) {
    // Write DRIVER_OK — starts an async Enable that will fail.
    transport.write_driver_ok();

    // stop() drains the failed enable and must call reset_status().
    transport.stop().await;

    assert_eq!(
        transport.read_status(),
        0,
        "status must be 0 after stop() completes a failed enable"
    );
    assert_eq!(
        transport.read_config_generation(),
        0,
        "config_generation must be 0 after stop() completes a failed enable"
    );
}

#[async_test]
async fn stop_during_failed_enable_resets_config_mmio(_driver: DefaultDriver) {
    let mut transport =
        MmioTestTransport::new(Box::new(PartialFailTestDevice::new(1, 0)), &_driver, 1);
    verify_stop_during_failed_enable_resets_config(&mut transport).await;
}

#[async_test]
async fn stop_during_failed_enable_resets_config_pci(_driver: DefaultDriver) {
    let mut transport =
        PciTestTransport::new(Box::new(PartialFailTestDevice::new(1, 0)), &_driver, 1);
    verify_stop_during_failed_enable_resets_config(&mut transport).await;
}
