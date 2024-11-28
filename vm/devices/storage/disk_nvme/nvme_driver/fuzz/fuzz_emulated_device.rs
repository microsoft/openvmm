// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A shim layer for an Emulated Device that provides passthrough to function calls
//! but can be configured to provide customized responses to function calls.

use crate::interrupt::DeviceInterrupt;
use crate::interrupt::DeviceInterruptSource;
use crate::memory::MappedDmaTarget;
use crate::memory::MemoryBlock;
use crate::memory::PAGE_SIZE;
use crate::DeviceBacking;
use crate::DeviceRegisterIo;
use crate::HostDmaAllocator;
use anyhow::Context;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pci::PciConfigSpace;
use guestmem::AlignedHeapMemory;
use guestmem::GuestMemory;
use guestmem::GuestMemoryAccess;
use inspect::Inspect;
use inspect::InspectMut;
use parking_lot::Mutex;
use pci_core::msi::MsiControl;
use pci_core::msi::MsiInterruptSet;
use pci_core::msi::MsiInterruptTarget;
use safeatomic::AtomicSliceOps;
use std::ptr::NonNull;
use std::sync::atomic::AtomicU8;
use std::sync::Arc;

/// An emulated device.
pub struct FuzzEmulatedDevice<T> {
    device: Arc<Mutex<T>>,
    controller: MsiController,
    shared_mem: DeviceSharedMemory,
}

impl<T: InspectMut> Inspect for FuzzEmulatedDevice<T> {
    fn inspect(&self, req: inspect::Request<'_>) {
        self.device.lock().inspect_mut(req);
    }
}

impl<T: PciConfigSpace + MmioIntercept> FuzzEmulatedDevice<T> {
    /// Creates a new emulated device, wrapping `device`, using the provided MSI controller.
    pub fn new(mut device: T, msi_set: MsiInterruptSet, shared_mem: DeviceSharedMemory) -> Self {
        EmulatedDevice::new(device, msi_set, shared_mem)
    }
}

impl<T: 'static + Send + InspectMut + MmioIntercept> DeviceBacking for FuzzEmulatedDevice<T> {
    type Registers = Mapping<T>;
    type DmaAllocator = EmulatedDmaAllocator;

    fn id(&self) -> &str {
        "emulated"
    }

    fn map_bar(&mut self, n: u8) -> anyhow::Result<Self::Registers> {
        Ok(Mapping {
            device: self.device.clone(),
            addr: (n as u64) << 32,
        })
    }

    /// Returns an object that can allocate host memory to be shared with the device.
    fn host_allocator(&self) -> Self::DmaAllocator {
        EmulatedDmaAllocator {
            shared_mem: self.shared_mem.clone(),
        }
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
