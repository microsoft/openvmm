// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This implements the user-mode driver device traits using an emulated PCI
//! device.

use crate::interrupt::DeviceInterrupt;
use crate::interrupt::DeviceInterruptSource;
use crate::memory::MappedDmaTarget;
use crate::memory::MemoryBlock;
use crate::memory::PAGE_SIZE;
use crate::DeviceBacking;
use crate::DeviceRegisterIo;
use crate::DmaClient;
use anyhow::Context;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pci::PciConfigSpace;
use guestmem::AlignedHeapMemory;
use guestmem::GuestMemory;
use guestmem::GuestMemoryAccess;
use inspect::Inspect;
use inspect::InspectMut;
use parking_lot::Mutex;
use pci_core::chipset_device_ext::PciChipsetDeviceExt;
use pci_core::msi::MsiControl;
use pci_core::msi::MsiInterruptSet;
use pci_core::msi::MsiInterruptTarget;
use safeatomic::AtomicSliceOps;
use std::ptr::NonNull;
use std::sync::atomic::AtomicU8;
use std::sync::Arc;

/// An emulated device.
pub struct EmulatedDevice<T> {
    device: Arc<Mutex<T>>,
    controller: MsiController,
    shared_mem: DeviceSharedMemory,
    bar0_len: usize,
}

impl<T: InspectMut> Inspect for EmulatedDevice<T> {
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

impl<T: PciConfigSpace + MmioIntercept> EmulatedDevice<T> {
    /// Creates a new emulated device, wrapping `device`, using the provided MSI controller.
    pub fn new(mut device: T, msi_set: MsiInterruptSet, shared_mem: DeviceSharedMemory) -> Self {
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
            shared_mem,
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

#[derive(Clone)]
pub struct DeviceSharedMemory {
    mem: GuestMemory,
    dma: GuestMemory,
    len: usize,
    state: Arc<Mutex<Vec<u64>>>,
}

struct Backing {
    mem: Arc<AlignedHeapMemory>,
    allow_dma: bool,
}

/// SAFETY: passing through to [`AlignedHeapMemory`].
unsafe impl GuestMemoryAccess for Backing {
    fn mapping(&self) -> Option<NonNull<u8>> {
        self.mem.mapping()
    }

    fn base_iova(&self) -> Option<u64> {
        self.allow_dma.then_some(0)
    }

    fn max_address(&self) -> u64 {
        self.mem.max_address()
    }
}

impl DeviceSharedMemory {
    pub fn new(size: usize, extra: usize) -> Self {
        assert_eq!(size % PAGE_SIZE, 0);
        assert_eq!(extra % PAGE_SIZE, 0);
        let mem_backing = Backing {
            mem: Arc::new(AlignedHeapMemory::new(size + extra)),
            allow_dma: false,
        };
        let dma_backing = Backing {
            mem: mem_backing.mem.clone(),
            allow_dma: true,
        };
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

    pub fn guest_memory(&self) -> &GuestMemory {
        &self.mem
    }

    pub fn guest_memory_for_driver_dma(&self) -> &GuestMemory {
        &self.dma
    }

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
        Some(DmaBuffer {
            mem: self.mem.clone(),
            pfns: pages,
            state: self.state.clone(),
        })
    }
}

pub struct DmaBuffer {
    mem: GuestMemory,
    pfns: Vec<u64>,
    state: Arc<Mutex<Vec<u64>>>,
}

impl Drop for DmaBuffer {
    fn drop(&mut self) {
        let mut state = self.state.lock();
        for &pfn in &self.pfns {
            state[pfn as usize / 64] &= !(1 << (pfn % 64));
        }
    }
}

/// SAFETY: we are handing out a VA and length for valid data, propagating the
/// guarantee from [`GuestMemory`] (which is known to be in a fully allocated
/// state because we used `GuestMemory::allocate` to create it).
unsafe impl MappedDmaTarget for DmaBuffer {
    fn base(&self) -> *const u8 {
        self.mem
            .full_mapping()
            .unwrap()
            .0
            .wrapping_add(self.pfns[0] as usize * PAGE_SIZE)
    }

    fn len(&self) -> usize {
        self.pfns.len() * PAGE_SIZE
    }

    fn pfns(&self) -> &[u64] {
        &self.pfns
    }

    fn pfn_bias(&self) -> u64 {
        0
    }
}

pub struct EmulatedDmaAllocator {
    shared_mem: DeviceSharedMemory,
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

impl<T: 'static + Send + InspectMut + MmioIntercept> DeviceBacking for EmulatedDevice<T> {
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
        Arc::new(EmulatedDmaAllocator {
            shared_mem: self.shared_mem.clone(),
        }) as Arc<dyn DmaClient>
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
