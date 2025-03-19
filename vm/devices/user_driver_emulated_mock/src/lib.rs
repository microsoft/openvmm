// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This crate provides a collection of wrapper structs around things like devices and memory. Through the wrappers, it provides functionality to emulate devices such
//! as Nvme and Mana and gives some additional control over things like [`GuestMemory`] to make testing devices easier.
//! Everything in this crate is meant for TESTING PURPOSES ONLY and it should only ever be added as a dev-dependency (Few expceptions like using this for fuzzing)
#![deny(missing_docs)]

mod dma_buffer;
pub mod guest_memory_access_wrapper;

use crate::dma_buffer::DmaBuffer;
use crate::guest_memory_access_wrapper::GuestMemoryAccessWrapper;

use anyhow::Context;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pci::PciConfigSpace;
use guestmem::AlignedHeapMemory;
use guestmem::GuestMemory;
use inspect::Inspect;
use inspect::InspectMut;
use parking_lot::Mutex;
use pci_core::chipset_device_ext::PciChipsetDeviceExt;
use pci_core::msi::MsiControl;
use pci_core::msi::MsiInterruptSet;
use pci_core::msi::MsiInterruptTarget;
use safeatomic::AtomicSliceOps;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use user_driver::DeviceBacking;
use user_driver::DeviceRegisterIo;
use user_driver::DmaClient;
use user_driver::interrupt::DeviceInterrupt;
use user_driver::interrupt::DeviceInterruptSource;
use user_driver::memory::MemoryBlock;
use user_driver::memory::PAGE_SIZE;

/// A wrapper around any user_driver device T. It provides device emulation by providing access to the memory shared with the device and thus
/// allowing the user to control device behaviour to a certain extent. Can be used with devices such as the `NvmeController`
pub struct EmulatedDevice<T, U> {
    device: Arc<Mutex<T>>,
    controller: MsiController,
    dma_client: Arc<U>,
    bar0_len: usize,
}

impl<T: InspectMut, U> Inspect for EmulatedDevice<T, U> {
    fn inspect(&self, req: inspect::Request<'_>) {
        self.device.lock().inspect_mut(req);
    }
}

struct MsiController {
    events: Arc<[DeviceInterruptSource]>,
}

impl MsiController {
    fn new(n: usize) -> Self {
        Self {
            events: (0..n).map(|_| DeviceInterruptSource::new()).collect(),
        }
    }
}

impl MsiInterruptTarget for MsiController {
    fn new_interrupt(&self) -> Box<dyn MsiControl> {
        let events = self.events.clone();
        Box::new(move |address, _data| {
            let index = address as usize;
            if let Some(event) = events.get(index) {
                tracing::debug!(index, "signaling interrupt");
                event.signal_uncached();
            } else {
                tracing::info!("interrupt ignored");
            }
        })
    }
}

impl<T: PciConfigSpace + MmioIntercept, U: DmaClient> EmulatedDevice<T, U> {
    /// Creates a new emulated device, wrapping `device` of type T, using the provided MSI Interrupt Set. Dma_client should point to memory
    /// shared with the device.
    pub fn new(mut device: T, msi_set: MsiInterruptSet, dma_client: Arc<U>) -> Self {
        // Connect an interrupt controller.
        let controller = MsiController::new(msi_set.len());
        msi_set.connect(&controller);

        let bars = device.probe_bar_masks();
        let bar0_len = !(bars[0] & !0xf) as usize + 1;

        // Enable BAR0 at 0, BAR4 at X.
        device.pci_cfg_write(0x20, 0).unwrap();
        device.pci_cfg_write(0x24, 0x1).unwrap();
        device
            .pci_cfg_write(
                0x4,
                pci_core::spec::cfg_space::Command::new()
                    .with_mmio_enabled(true)
                    .into_bits() as u32,
            )
            .unwrap();

        // Enable MSIX.
        for i in 0u64..64 {
            device
                .mmio_write((0x1 << 32) + i * 16, &i.to_ne_bytes())
                .unwrap();
            device
                .mmio_write((0x1 << 32) + i * 16 + 12, &0u32.to_ne_bytes())
                .unwrap();
        }
        device.pci_cfg_write(0x40, 0x80000000).unwrap();

        Self {
            device: Arc::new(Mutex::new(device)),
            controller,
            dma_client,
            bar0_len,
        }
    }
}

/// A memory mapping for an [`EmulatedDevice`].
#[derive(Inspect)]
pub struct Mapping<T> {
    #[inspect(skip)]
    device: Arc<Mutex<T>>,
    addr: u64,
    len: usize,
}

#[repr(C, align(4096))]
struct Page([AtomicU8; PAGE_SIZE]);

impl Default for Page {
    fn default() -> Self {
        Self([0; PAGE_SIZE].map(AtomicU8::new))
    }
}

/// Some synthetic (test) memory that can be shared with an [`EmulatedDevice`]. It provides both shared and dma-able memory to the device
/// and uses [`GuestMemory`] backed by [`AlignedHeapMemory`].
#[derive(Clone)]
pub struct DeviceSharedMemory {
    mem: GuestMemory,
    dma: GuestMemory,
    len: usize,
    state: Arc<Mutex<Vec<u64>>>,
}

impl DeviceSharedMemory {
    /// Creates a new [`DeviceSharedMemory`] object. First "size" pages are alloacted as regular
    /// memory and the "extra" is strictly for dma testing. Both inputs are in bytes and required
    /// to be page aligned.
    pub fn new(size: usize, extra: usize) -> Self {
        assert_eq!(size % PAGE_SIZE, 0);
        assert_eq!(extra % PAGE_SIZE, 0);
        let mem_backing =
            GuestMemoryAccessWrapper::new(Arc::new(AlignedHeapMemory::new(size + extra)), false);
        let dma_backing = GuestMemoryAccessWrapper::new(mem_backing.mem().clone(), true);
        let mem = GuestMemory::new("emulated_shared_mem", mem_backing);
        let dma = GuestMemory::new("emulated_shared_dma", dma_backing);
        let len = size / PAGE_SIZE;
        Self {
            mem,
            dma,
            len,
            state: Arc::new(Mutex::new(vec![0; (len + 63) / 64])),
        }
    }

    /// Gets regular [`GuestMemory`]
    pub fn guest_memory(&self) -> &GuestMemory {
        &self.mem
    }

    /// Gets dma-able [`GuestMemory`]
    pub fn guest_memory_for_driver_dma(&self) -> &GuestMemory {
        &self.dma
    }

    /// Allocates `len` number of contiguous bytes in `mem`. Input must be page aligned
    pub fn alloc(&self, len: usize) -> Option<DmaBuffer> {
        assert!(len % PAGE_SIZE == 0);
        let count = len / PAGE_SIZE;

        // Find a contiguous free range by scanning the state bitmap.
        let start_page = {
            let mut state = self.state.lock();
            let mut i = 0;
            let mut contig = 0;
            while contig < count && i < self.len {
                if state[i / 64] & 1 << (i % 64) != 0 {
                    contig = 0;
                } else {
                    contig += 1;
                }
                i += 1;
            }
            if contig < count {
                return None;
            }
            let start = i - contig;
            for j in start..i {
                state[j / 64] |= 1 << (j % 64);
            }
            start
        };

        let pages = (start_page..start_page + count).map(|p| p as u64).collect();
        Some(DmaBuffer::new(self.mem.clone(), pages, self.state.clone()))
    }
}

/// Implements a [`DmaClient`] backed by [`DeviceSharedMemory`]
#[derive(Inspect)]
pub struct EmulatedDmaAllocator {
    #[inspect(skip)]
    shared_mem: DeviceSharedMemory,
}

impl EmulatedDmaAllocator {
    /// Returns a new EmulatedDmaAllocator struct wrapping the provided [`DeviceSharedMemory`]
    pub fn new(shared_mem: DeviceSharedMemory) -> Self {
        Self { shared_mem }
    }
}

impl DmaClient for EmulatedDmaAllocator {
    fn allocate_dma_buffer(&self, len: usize) -> anyhow::Result<MemoryBlock> {
        let memory = MemoryBlock::new(self.shared_mem.alloc(len).context("out of memory")?);
        memory.as_slice().atomic_fill(0);
        Ok(memory)
    }

    fn attach_dma_buffer(&self, _len: usize, _base_pfn: u64) -> anyhow::Result<MemoryBlock> {
        anyhow::bail!("restore is not supported for emulated DMA")
    }
}

impl<T: 'static + Send + InspectMut + MmioIntercept, U: 'static + Send + DmaClient> DeviceBacking
    for EmulatedDevice<T, U>
{
    type Registers = Mapping<T>;

    fn id(&self) -> &str {
        "emulated"
    }

    fn map_bar(&mut self, n: u8) -> anyhow::Result<Self::Registers> {
        if n != 0 {
            anyhow::bail!("invalid bar {n}");
        }
        Ok(Mapping {
            device: self.device.clone(),
            addr: (n as u64) << 32,
            len: self.bar0_len,
        })
    }

    fn dma_client(&self) -> Arc<dyn DmaClient> {
        self.dma_client.clone()
    }

    fn max_interrupt_count(&self) -> u32 {
        self.controller.events.len() as u32
    }

    fn map_interrupt(&mut self, msix: u32, _cpu: u32) -> anyhow::Result<DeviceInterrupt> {
        Ok(self
            .controller
            .events
            .get(msix as usize)
            .with_context(|| format!("invalid msix index {msix}"))?
            .new_target())
    }
}

impl<T: MmioIntercept + Send> DeviceRegisterIo for Mapping<T> {
    fn len(&self) -> usize {
        self.len
    }

    fn read_u32(&self, offset: usize) -> u32 {
        let mut n = [0; 4];
        self.device
            .lock()
            .mmio_read(self.addr + offset as u64, &mut n)
            .unwrap();
        u32::from_ne_bytes(n)
    }

    fn read_u64(&self, offset: usize) -> u64 {
        let mut n = [0; 8];
        self.device
            .lock()
            .mmio_read(self.addr + offset as u64, &mut n)
            .unwrap();
        u64::from_ne_bytes(n)
    }

    fn write_u32(&self, offset: usize, data: u32) {
        self.device
            .lock()
            .mmio_write(self.addr + offset as u64, &data.to_ne_bytes())
            .unwrap();
    }

    fn write_u64(&self, offset: usize, data: u64) {
        self.device
            .lock()
            .mmio_write(self.addr + offset as u64, &data.to_ne_bytes())
            .unwrap();
    }
}
