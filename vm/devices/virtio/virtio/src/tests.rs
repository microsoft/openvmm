// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: test code implements a custom `GuestMemory` backing, which requires
// unsafe.
#![expect(unsafe_code)]
#![cfg(test)]

use crate::DeviceTraits;
use crate::LegacyVirtioDevice;
use crate::LegacyWrapper;
use crate::PciInterruptModel;
use crate::QueueResources;
use crate::VirtioQueueCallbackWork;
use crate::VirtioQueueState;
use crate::VirtioQueueWorker;
use crate::VirtioQueueWorkerContext;
use crate::VirtioState;
use crate::queue::QueueParams;
use crate::spec::pci::*;
use crate::spec::queue::*;
use crate::spec::*;
use crate::transport::VirtioMmioDevice;
use crate::transport::VirtioPciDevice;
use async_trait::async_trait;
use chipset_device::mmio::ExternallyManagedMmioIntercepts;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pci::PciConfigSpace;
use futures::StreamExt;
use guestmem::DoorbellRegistration;
use guestmem::GuestMemory;
use guestmem::GuestMemoryAccess;
use guestmem::GuestMemoryBackingError;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_async::timer::PolledTimer;
use pal_event::Event;
use parking_lot::Mutex;
use pci_core::msi::MsiConnection;
use pci_core::spec::caps::CapabilityId;
use pci_core::spec::cfg_space;
use pci_core::test_helpers::TestPciInterruptController;
use std::collections::BTreeMap;
use std::future::poll_fn;
use std::io;
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::Duration;
use task_control::TaskControl;
use test_with_tracing::test;
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
const _VIRTIO_F_RING_PACKED: u32 = 4;

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

#[derive(Default)]
struct VirtioTestMemoryAccess {
    memory_map: Mutex<MemoryMap>,
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
            None => panic!("Unexpected read request at address {:x}", address),
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
            _ => panic!("Unexpected write request at address {:x}", address),
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
impl Drop for DoorbellEntry {
    fn drop(&mut self) {}
}

impl DoorbellRegistration for VirtioTestMemoryAccess {
    fn register_doorbell(
        &self,
        _: u64,
        _: Option<u64>,
        _: Option<u32>,
        _: &Event,
    ) -> io::Result<Box<dyn Send + Sync>> {
        Ok(Box::new(DoorbellEntry))
    }
}

type VirtioTestWorkCallback =
    Box<dyn Fn(anyhow::Result<VirtioQueueCallbackWork>) -> bool + Sync + Send>;
struct CreateDirectQueueParams {
    process_work: VirtioTestWorkCallback,
    notify: Interrupt,
    event: Event,
}

struct VirtioTestGuest {
    test_mem: Arc<VirtioTestMemoryAccess>,
    driver: DefaultDriver,
    num_queues: u16,
    queue_size: u16,
    allow_indirect_descriptors: bool,
    use_ring_event_index: bool,
    last_avail_index: Vec<u16>,
    last_used_index: Vec<u16>,
    avail_descriptors: Vec<Vec<bool>>,
    exit_event: event_listener::Event,
}

impl VirtioTestGuest {
    fn new(
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
            use_ring_event_index,
            last_avail_index,
            last_used_index,
            avail_descriptors,
            exit_event: event_listener::Event::new(),
        };
        for i in 0..num_queues {
            test_guest.add_queue_memory(i);
        }
        test_guest
    }

    fn mem(&self) -> GuestMemory {
        GuestMemory::new("test", self.test_mem.clone())
    }

    fn create_direct_queues<F>(&self, f: F) -> Vec<TaskControl<VirtioQueueWorker, VirtioQueueState>>
    where
        F: Fn(u16) -> CreateDirectQueueParams,
    {
        (0..self.num_queues)
            .map(|i| {
                let params = f(i);
                let worker = VirtioQueueWorker::new(
                    self.driver.clone(),
                    Box::new(VirtioTestWork {
                        callback: params.process_work,
                    }),
                );
                worker.into_running_task(
                    "virtio-test-queue".to_string(),
                    self.mem(),
                    self.queue_features(),
                    QueueResources {
                        params: self.queue_params(i),
                        notify: params.notify,
                        event: params.event,
                    },
                    self.exit_event.listen(),
                )
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

    fn setup_chipset_device(
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
        assert_eq!(dev.read_u32(0xfc), 2);
    }

    fn setup_pci_device(
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
        // run device
        device_status = VIRTIO_DRIVER_OK as u8;
        dev.pci_device
            .mmio_write(bar_address1 + 20, &device_status.to_le_bytes())
            .unwrap();
        let mut config_generation: [u8; 1] = [0];
        dev.pci_device
            .mmio_read(bar_address1 + 21, &mut config_generation)
            .unwrap();
        assert_eq!(config_generation[0], 2);
    }

    fn get_queue_descriptor(&self, queue_index: u16, descriptor_index: u16) -> u64 {
        self.get_queue_descriptor_base_address(queue_index) + 0x10 * descriptor_index as u64
    }

    fn add_queue_memory(&self, queue_index: u16) {
        // descriptors
        for i in 0..self.queue_size {
            let base = self.get_queue_descriptor(queue_index, i);
            // physical address
            self.test_mem.modify_memory_map(
                base,
                &(self.get_queue_descriptor_backing_memory_address(queue_index)
                    + 0x1000 * i as u64)
                    .to_le_bytes(),
                false,
            );
            // length
            self.test_mem
                .modify_memory_map(base + 8, &0x1000u32.to_le_bytes(), false);
            // flags
            self.test_mem
                .modify_memory_map(base + 12, &0u16.to_le_bytes(), false);
            // next index
            self.test_mem
                .modify_memory_map(base + 14, &0u16.to_le_bytes(), false);
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

    fn reserve_descriptor(&mut self, queue_index: u16) -> u16 {
        let avail_descriptors = &mut self.avail_descriptors[queue_index as usize];
        for (i, desc) in avail_descriptors.iter_mut().enumerate() {
            if *desc {
                *desc = false;
                return i as u16;
            }
        }

        panic!("No descriptors are available!");
    }

    fn free_descriptor(&mut self, queue_index: u16, desc_index: u16) {
        assert!(desc_index < self.queue_size);
        let desc_addr = self.get_queue_descriptor(queue_index, desc_index);
        let flags: DescriptorFlags = self.test_mem.memory_map_get_u16(desc_addr + 12).into();
        if flags.next() {
            let next = self.test_mem.memory_map_get_u16(desc_addr + 14);
            self.free_descriptor(queue_index, next);
        }
        let avail_descriptors = &mut self.avail_descriptors[queue_index as usize];
        assert_eq!(avail_descriptors[desc_index as usize], false);
        avail_descriptors[desc_index as usize] = true;
    }

    fn queue_available_desc(&mut self, queue_index: u16, desc_index: u16) {
        let avail_base_addr = self.get_queue_available_base_address(queue_index);
        let last_avail_index = &mut self.last_avail_index[queue_index as usize];
        let next_index = *last_avail_index % self.queue_size;
        *last_avail_index = last_avail_index.wrapping_add(1);
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
        let next_descriptor = self.reserve_descriptor(queue_index);
        // flags
        self.test_mem.modify_memory_map(
            self.get_queue_descriptor(queue_index, next_descriptor) + 12,
            &0u16.to_le_bytes(),
            false,
        );
        self.queue_available_desc(queue_index, next_descriptor);
    }

    fn add_indirect_to_avail_queue(&mut self, queue_index: u16) {
        self.add_indirect_to_avail_queue_with_length(queue_index, 0x1000);
    }

    fn add_indirect_to_avail_queue_with_length(&mut self, queue_index: u16, indirect_length: u32) {
        let next_descriptor = self.reserve_descriptor(queue_index);
        // flags
        self.test_mem.modify_memory_map(
            self.get_queue_descriptor(queue_index, next_descriptor) + 12,
            &u16::from(DescriptorFlags::new().with_indirect(true)).to_le_bytes(),
            false,
        );
        // create another (indirect) descriptor in the buffer
        let buffer_addr = self.get_queue_descriptor_backing_memory_address(queue_index);
        // physical address
        self.test_mem
            .modify_memory_map(buffer_addr, &0xffffffff00000000u64.to_le_bytes(), false);
        // length
        self.test_mem
            .modify_memory_map(buffer_addr + 8, &indirect_length.to_le_bytes(), false);
        // flags
        self.test_mem
            .modify_memory_map(buffer_addr + 12, &0u16.to_le_bytes(), false);
        // next index
        self.test_mem
            .modify_memory_map(buffer_addr + 14, &0u16.to_le_bytes(), false);
        self.queue_available_desc(queue_index, next_descriptor);
    }

    fn add_linked_to_avail_queue(&mut self, queue_index: u16, desc_count: u16) {
        let mut descriptors = Vec::with_capacity(desc_count as usize);
        for _ in 0..desc_count {
            descriptors.push(self.reserve_descriptor(queue_index));
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
        let next_descriptor = self.reserve_descriptor(queue_index);
        // flags
        self.test_mem.modify_memory_map(
            self.get_queue_descriptor(queue_index, next_descriptor) + 12,
            &u16::from(DescriptorFlags::new().with_indirect(true)).to_le_bytes(),
            false,
        );
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
            // flags
            let flags = if i < desc_count - 1 {
                u16::from(DescriptorFlags::new().with_next(true))
            } else {
                0
            };
            self.test_mem
                .modify_memory_map(base + 12, &flags.to_le_bytes(), false);
            // next index
            let next = if i < desc_count - 1 { i + 1 } else { 0 };
            self.test_mem
                .modify_memory_map(base + 14, &next.to_le_bytes(), false);
        }
        self.queue_available_desc(queue_index, next_descriptor);
    }

    fn get_next_completed(&mut self, queue_index: u16) -> Option<(u16, u32)> {
        let avail_base_addr = self.get_queue_available_base_address(queue_index);
        let used_base_addr = self.get_queue_used_base_address(queue_index);
        let cur_used_index = self.test_mem.memory_map_get_u16(used_base_addr + 2);
        let last_used_index = &mut self.last_used_index[queue_index as usize];
        if *last_used_index == cur_used_index {
            return None;
        }

        if self.use_ring_event_index {
            self.test_mem.modify_memory_map(
                avail_base_addr + 4 + 2 * self.queue_size as u64,
                &cur_used_index.to_le_bytes(),
                false,
            );
        }

        let next_index = *last_used_index % self.queue_size;
        *last_used_index = last_used_index.wrapping_add(1);
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
}

struct VirtioTestWork {
    callback: VirtioTestWorkCallback,
}

#[async_trait]
impl VirtioQueueWorkerContext for VirtioTestWork {
    async fn process_work(&mut self, work: anyhow::Result<VirtioQueueCallbackWork>) -> bool {
        (self.callback)(work)
    }
}
struct VirtioPciTestDevice {
    pci_device: VirtioPciDevice,
    test_intc: Arc<TestPciInterruptController>,
}

type TestDeviceQueueWorkFn = Arc<dyn Fn(u16, VirtioQueueCallbackWork) + Send + Sync>;

struct TestDevice {
    traits: DeviceTraits,
    queue_work: Option<TestDeviceQueueWorkFn>,
}

impl TestDevice {
    fn new(traits: DeviceTraits, queue_work: Option<TestDeviceQueueWorkFn>) -> Self {
        Self { traits, queue_work }
    }
}

impl LegacyVirtioDevice for TestDevice {
    fn traits(&self) -> DeviceTraits {
        self.traits.clone()
    }

    fn read_registers_u32(&self, _offset: u16) -> u32 {
        0
    }

    fn write_registers_u32(&mut self, _offset: u16, _val: u32) {}

    fn get_work_callback(&mut self, index: u16) -> Box<dyn VirtioQueueWorkerContext + Send> {
        Box::new(TestDeviceWorker {
            index,
            queue_work: self.queue_work.clone(),
        })
    }

    fn state_change(&mut self, _state: &VirtioState) {}
}

struct TestDeviceWorker {
    index: u16,
    queue_work: Option<TestDeviceQueueWorkFn>,
}

#[async_trait]
impl VirtioQueueWorkerContext for TestDeviceWorker {
    async fn process_work(&mut self, work: anyhow::Result<VirtioQueueCallbackWork>) -> bool {
        if let Err(err) = work {
            panic!(
                "Invalid virtio queue state index {} error {}",
                self.index,
                err.as_ref() as &dyn std::error::Error
            );
        }
        if let Some(ref func) = self.queue_work {
            (func)(self.index, work.unwrap());
        }
        true
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
        let msi_conn = MsiConnection::new();

        let dev = VirtioPciDevice::new(
            Box::new(LegacyWrapper::new(
                &VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
                TestDevice::new(
                    DeviceTraits {
                        device_id: 3,
                        device_features: VirtioDeviceFeatures::new().with_bank(0, 2),
                        max_queues: num_queues,
                        device_register_length: 12,
                        ..Default::default()
                    },
                    queue_work,
                ),
                &mem,
            )),
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
    let mem = VirtioTestMemoryAccess::new();
    let doorbell_registration: Arc<dyn DoorbellRegistration> = mem.clone();
    let mem = GuestMemory::new("test", mem);
    let interrupt = LineInterrupt::detached();

    let mut dev = VirtioMmioDevice::new(
        Box::new(LegacyWrapper::new(
            &VmTaskDriverSource::new(SingleDriverBackend::new(driver)),
            TestDevice::new(
                DeviceTraits {
                    device_id: 3,
                    device_features: VirtioDeviceFeatures::new().with_bank(0, 2),
                    max_queues: 1,
                    device_register_length: 0,
                    ..Default::default()
                },
                None,
            ),
            &mem,
        )),
        interrupt,
        Some(doorbell_registration),
        0,
        1,
    );
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
    assert_eq!(dev.read_u32(16), VIRTIO_F_VERSION_1);
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
    assert_eq!(dev.read_u32(32), VIRTIO_F_VERSION_1);
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
    assert_eq!(dev.read_u32(52), 0x40);
    // queue size (queue 0)
    assert_eq!(dev.read_u32(56), 0x40);
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
    assert_eq!(header[3], VIRTIO_PCI_CAP_COMMON_CFG);
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
    assert_eq!(header[3], VIRTIO_PCI_CAP_NOTIFY_CFG);
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
    assert_eq!(header[3], VIRTIO_PCI_CAP_ISR_CFG);
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
    assert_eq!(header[3], VIRTIO_PCI_CAP_DEVICE_CFG);
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
        VIRTIO_F_VERSION_1
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
        VIRTIO_F_VERSION_1
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
    assert_eq!(pci_test_device.read_u32(bar_address1 + 24), 0x40);
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
    assert_eq!(pci_test_device.read_u32(bar_address1 + 20), 1 << 24);
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

#[async_test]
async fn verify_queue_simple(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    let mut guest = VirtioTestGuest::new(&driver, &test_mem, 1, 2, true);
    let base_addr = guest.get_queue_descriptor_backing_memory_address(0);
    let (tx, mut rx) = mesh::mpsc_channel();
    let event = Event::new();
    let mut queues = guest.create_direct_queues(|i| {
        let tx = tx.clone();
        CreateDirectQueueParams {
            process_work: Box::new(move |work: anyhow::Result<VirtioQueueCallbackWork>| {
                let mut work = work.expect("Queue failure");
                assert_eq!(work.payload.len(), 1);
                assert_eq!(work.payload[0].length, 0x1000);
                match work.payload[0].address {
                    addr if addr == base_addr => work.complete(123),
                    addr if addr == base_addr + 0x1000 => work.complete(456),
                    _ => panic!("Unexpected address {}", work.payload[0].address),
                }
                true
            }),
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
async fn verify_queue_indirect(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    let mut guest = VirtioTestGuest::new(&driver, &test_mem, 1, 2, true);
    let base_addr = guest.get_queue_descriptor_backing_memory_address(0);
    let (tx, mut rx) = mesh::mpsc_channel();
    let event = Event::new();
    let mut queues = guest.create_direct_queues(|i| {
        let tx = tx.clone();
        CreateDirectQueueParams {
            process_work: Box::new(move |work: anyhow::Result<VirtioQueueCallbackWork>| {
                let mut work = work.expect("Queue failure");
                assert_eq!(work.payload.len(), 1);
                assert_eq!(work.payload[0].length, 0x1000);
                match work.payload[0].address {
                    0xffffffff00000000u64 => work.complete(123),
                    addr if addr == base_addr + 0x1000 => work.complete(456),
                    _ => panic!("Unexpected address {}", work.payload[0].address),
                }
                true
            }),
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
async fn verify_queue_linked(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    let mut guest = VirtioTestGuest::new(&driver, &test_mem, 1, 8, true);
    let (tx, mut rx) = mesh::mpsc_channel();
    let base_address = guest.get_queue_descriptor_backing_memory_address(0);
    let event = Event::new();
    let mut queues = guest.create_direct_queues(|i| {
        let tx = tx.clone();
        CreateDirectQueueParams {
            process_work: Box::new(move |work: anyhow::Result<VirtioQueueCallbackWork>| {
                let mut work = work.expect("Queue failure");
                if work.payload.len() == 3 {
                    for i in 0..work.payload.len() {
                        assert_eq!(work.payload[i].address, base_address + 0x1000 * i as u64);
                        assert_eq!(work.payload[i].length, 0x1000);
                    }
                    work.complete(123);
                } else {
                    assert_eq!(work.payload.len(), 1);
                    work.complete(456);
                }
                true
            }),
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
async fn verify_queue_indirect_linked(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    let mut guest = VirtioTestGuest::new(&driver, &test_mem, 1, 8, true);
    let (tx, mut rx) = mesh::mpsc_channel();
    let event = Event::new();
    let mut queues = guest.create_direct_queues(|i| {
        let tx = tx.clone();
        CreateDirectQueueParams {
            process_work: Box::new(move |work: anyhow::Result<VirtioQueueCallbackWork>| {
                let mut work = work.expect("Queue failure");
                if work.payload.len() == 3 {
                    for i in 0..work.payload.len() {
                        assert_eq!(
                            work.payload[i].address,
                            0xffffffff00000000u64 + 0x1000 * i as u64
                        );
                        assert_eq!(work.payload[i].length, 0x1000);
                    }
                    work.complete(123);
                } else {
                    assert_eq!(work.payload.len(), 1);
                    work.complete(456);
                }
                true
            }),
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
async fn verify_queue_avail_rollover(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    let mut guest = VirtioTestGuest::new(&driver, &test_mem, 1, 2, true);
    let base_addr = guest.get_queue_descriptor_backing_memory_address(0);
    let (tx, mut rx) = mesh::mpsc_channel();
    let event = Event::new();
    let mut queues = guest.create_direct_queues(|i| {
        let tx = tx.clone();
        CreateDirectQueueParams {
            process_work: Box::new(move |work: anyhow::Result<VirtioQueueCallbackWork>| {
                let mut work = work.expect("Queue failure");
                assert_eq!(work.payload.len(), 1);
                assert_eq!(work.payload[0].address, base_addr);
                assert_eq!(work.payload[0].length, 0x1000);
                work.complete(123);
                true
            }),
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
        assert_eq!(desc, 0u16);
        assert_eq!(len, 123);
        assert_eq!(guest.get_next_completed(0).is_none(), true);
    }

    queues[0].stop().await;
}

#[async_test]
async fn verify_multi_queue(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    let mut guest = VirtioTestGuest::new(&driver, &test_mem, 5, 2, true);
    let (tx, mut rx) = mesh::mpsc_channel();
    let events = (0..guest.num_queues)
        .map(|_| Event::new())
        .collect::<Vec<_>>();
    let mut queues = guest.create_direct_queues(|queue_index| {
        let tx = tx.clone();
        let base_addr = guest.get_queue_descriptor_backing_memory_address(queue_index);
        CreateDirectQueueParams {
            process_work: Box::new(move |work: anyhow::Result<VirtioQueueCallbackWork>| {
                let mut work = work.expect("Queue failure");
                assert_eq!(work.payload.len(), 1);
                assert_eq!(work.payload[0].address, base_addr);
                assert_eq!(work.payload[0].length, 0x1000);
                work.complete(123 * queue_index as u32);
                true
            }),
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

#[async_test]
async fn verify_device_queue_simple(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    let doorbell_registration: Arc<dyn DoorbellRegistration> = test_mem.clone();
    let mut guest = VirtioTestGuest::new(&driver, &test_mem, 1, 2, true);
    let mem = guest.mem();
    let features = VirtioDeviceFeatures::new()
        .with_bank(0, VIRTIO_F_RING_EVENT_IDX | 2)
        .with_bank(1, VIRTIO_F_VERSION_1);
    let target = TestLineInterruptTarget::new_arc();
    let interrupt = LineInterrupt::new_with_target("test", target.clone(), 0);
    let base_addr = guest.get_queue_descriptor_backing_memory_address(0);
    let queue_work = Arc::new(move |_: u16, mut work: VirtioQueueCallbackWork| {
        assert_eq!(work.payload.len(), 1);
        assert_eq!(work.payload[0].address, base_addr);
        assert_eq!(work.payload[0].length, 0x1000);
        work.complete(123);
    });
    let mut dev = VirtioMmioDevice::new(
        Box::new(LegacyWrapper::new(
            &VmTaskDriverSource::new(SingleDriverBackend::new(driver)),
            TestDevice::new(
                DeviceTraits {
                    device_id: 3,
                    device_features: features.clone(),
                    max_queues: 1,
                    device_register_length: 0,
                    ..Default::default()
                },
                Some(queue_work),
            ),
            &mem,
        )),
        interrupt,
        Some(doorbell_registration),
        0,
        1,
    );

    guest.setup_chipset_device(&mut dev, features);
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
async fn verify_device_multi_queue(driver: DefaultDriver) {
    let num_queues = 5;
    let test_mem = VirtioTestMemoryAccess::new();
    let doorbell_registration: Arc<dyn DoorbellRegistration> = test_mem.clone();
    let mut guest = VirtioTestGuest::new(&driver, &test_mem, num_queues, 2, true);
    let mem = guest.mem();
    let features = VirtioDeviceFeatures::new()
        .with_bank(0, VIRTIO_F_RING_EVENT_IDX | 2)
        .with_bank(1, VIRTIO_F_VERSION_1);
    let target = TestLineInterruptTarget::new_arc();
    let interrupt = LineInterrupt::new_with_target("test", target.clone(), 0);
    let base_addr: Vec<_> = (0..num_queues)
        .map(|i| guest.get_queue_descriptor_backing_memory_address(i))
        .collect();
    let queue_work = Arc::new(move |i: u16, mut work: VirtioQueueCallbackWork| {
        assert_eq!(work.payload.len(), 1);
        assert_eq!(work.payload[0].address, base_addr[i as usize]);
        assert_eq!(work.payload[0].length, 0x1000);
        work.complete(123 * i as u32);
    });
    let mut dev = VirtioMmioDevice::new(
        Box::new(LegacyWrapper::new(
            &VmTaskDriverSource::new(SingleDriverBackend::new(driver)),
            TestDevice::new(
                DeviceTraits {
                    device_id: 3,
                    device_features: features.clone(),
                    max_queues: num_queues + 1,
                    device_register_length: 0,
                    ..Default::default()
                },
                Some(queue_work),
            ),
            &mem,
        )),
        interrupt,
        Some(doorbell_registration),
        0,
        1,
    );
    guest.setup_chipset_device(&mut dev, features);
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
async fn verify_device_multi_queue_pci(driver: DefaultDriver) {
    let num_queues = 5;
    let test_mem = VirtioTestMemoryAccess::new();
    let mut guest = VirtioTestGuest::new(&driver, &test_mem, num_queues, 2, true);
    let features = VirtioDeviceFeatures::new()
        .with_bank(0, VIRTIO_F_RING_EVENT_IDX | 2)
        .with_bank(1, VIRTIO_F_VERSION_1);
    let base_addr: Vec<_> = (0..num_queues)
        .map(|i| guest.get_queue_descriptor_backing_memory_address(i))
        .collect();
    let mut dev = VirtioPciTestDevice::new(
        &driver,
        num_queues + 1,
        &test_mem,
        Some(Arc::new(move |i, mut work| {
            assert_eq!(work.payload.len(), 1);
            assert_eq!(work.payload[0].address, base_addr[i as usize]);
            assert_eq!(work.payload[0].length, 0x1000);
            work.complete(123 * i as u32);
        })),
    );

    guest.setup_pci_device(&mut dev, features);

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
    // reset the device
    let device_status: u8 = 0;
    dev.pci_device
        .mmio_write(0x10000000000 + 20, &device_status.to_le_bytes())
        .unwrap();
    drop(dev);
}

#[test]
fn indirect_descriptor_zero_length_rejected() {
    let test_mem = VirtioTestMemoryAccess::new();
    let queue_size: u16 = 4;
    let desc_base: u64 = 0x1000;
    let avail_base: u64 = 0x2000;
    let used_base: u64 = 0x3000;
    let buffer_base: u64 = 0x4000;

    // Set up descriptor 0: indirect flag, length = 0 (invalid)
    let desc_addr = desc_base;
    test_mem.modify_memory_map(desc_addr, &buffer_base.to_le_bytes(), false); // address
    test_mem.modify_memory_map(desc_addr + 8, &0u32.to_le_bytes(), false); // length = 0
    test_mem.modify_memory_map(
        desc_addr + 12,
        &u16::from(DescriptorFlags::new().with_indirect(true)).to_le_bytes(),
        false,
    ); // flags
    test_mem.modify_memory_map(desc_addr + 14, &0u16.to_le_bytes(), false); // next

    // Fill remaining descriptors so the desc memory region is contiguous
    for i in 1..queue_size {
        let addr = desc_base + 0x10 * i as u64;
        test_mem.modify_memory_map(addr, &[0u8; 16], false);
    }

    // Set up available ring: flags=0, idx=1, ring[0]=0
    test_mem.modify_memory_map(avail_base, &0u16.to_le_bytes(), false); // flags
    test_mem.modify_memory_map(avail_base + 2, &1u16.to_le_bytes(), false); // idx = 1
    test_mem.modify_memory_map(avail_base + 4, &0u16.to_le_bytes(), false); // ring[0] = descriptor 0
    for i in 1..queue_size {
        test_mem.modify_memory_map(avail_base + 4 + 2 * i as u64, &0u16.to_le_bytes(), false);
    }

    // Set up used ring
    test_mem.modify_memory_map(used_base, &0u16.to_le_bytes(), true); // flags
    test_mem.modify_memory_map(used_base + 2, &0u16.to_le_bytes(), true); // idx
    for i in 0..queue_size {
        let addr = used_base + 4 + 8 * i as u64;
        test_mem.modify_memory_map(addr, &0u32.to_le_bytes(), true); // id
        test_mem.modify_memory_map(addr + 4, &0u32.to_le_bytes(), true); // len
    }

    let mem = GuestMemory::new("test", test_mem);
    let features = VirtioDeviceFeatures::new()
        .with_bank0(VirtioDeviceFeaturesBank0::new().with_ring_indirect_desc(true));
    let params = QueueParams {
        size: queue_size,
        enable: true,
        desc_addr: desc_base,
        avail_addr: avail_base,
        used_addr: used_base,
    };
    let mut queue = crate::queue::QueueCoreGetWork::new(features, mem, params).unwrap();
    let result = queue.try_next_work();
    assert!(
        matches!(
            result,
            Err(crate::queue::QueueError::InvalidIndirectSize(0))
        ),
        "expected InvalidIndirectSize(0)"
    );
}

#[test]
fn indirect_descriptor_misaligned_length_rejected() {
    let test_mem = VirtioTestMemoryAccess::new();
    let queue_size: u16 = 4;
    let desc_base: u64 = 0x1000;
    let avail_base: u64 = 0x2000;
    let used_base: u64 = 0x3000;
    let buffer_base: u64 = 0x4000;
    let misaligned_len: u32 = 17; // not a multiple of 16

    // Set up descriptor 0: indirect flag, length = 17 (misaligned)
    let desc_addr = desc_base;
    test_mem.modify_memory_map(desc_addr, &buffer_base.to_le_bytes(), false);
    test_mem.modify_memory_map(desc_addr + 8, &misaligned_len.to_le_bytes(), false);
    test_mem.modify_memory_map(
        desc_addr + 12,
        &u16::from(DescriptorFlags::new().with_indirect(true)).to_le_bytes(),
        false,
    );
    test_mem.modify_memory_map(desc_addr + 14, &0u16.to_le_bytes(), false);

    for i in 1..queue_size {
        let addr = desc_base + 0x10 * i as u64;
        test_mem.modify_memory_map(addr, &[0u8; 16], false);
    }

    // Available ring
    test_mem.modify_memory_map(avail_base, &0u16.to_le_bytes(), false);
    test_mem.modify_memory_map(avail_base + 2, &1u16.to_le_bytes(), false);
    test_mem.modify_memory_map(avail_base + 4, &0u16.to_le_bytes(), false);
    for i in 1..queue_size {
        test_mem.modify_memory_map(avail_base + 4 + 2 * i as u64, &0u16.to_le_bytes(), false);
    }

    // Used ring
    test_mem.modify_memory_map(used_base, &0u16.to_le_bytes(), true);
    test_mem.modify_memory_map(used_base + 2, &0u16.to_le_bytes(), true);
    for i in 0..queue_size {
        let addr = used_base + 4 + 8 * i as u64;
        test_mem.modify_memory_map(addr, &0u32.to_le_bytes(), true);
        test_mem.modify_memory_map(addr + 4, &0u32.to_le_bytes(), true);
    }

    let mem = GuestMemory::new("test", test_mem);
    let features = VirtioDeviceFeatures::new()
        .with_bank0(VirtioDeviceFeaturesBank0::new().with_ring_indirect_desc(true));
    let params = QueueParams {
        size: queue_size,
        enable: true,
        desc_addr: desc_base,
        avail_addr: avail_base,
        used_addr: used_base,
    };
    let mut queue = crate::queue::QueueCoreGetWork::new(features, mem, params).unwrap();
    let result = queue.try_next_work();
    assert!(
        matches!(
            result,
            Err(crate::queue::QueueError::InvalidIndirectSize(17))
        ),
        "expected InvalidIndirectSize(17)"
    );
}
