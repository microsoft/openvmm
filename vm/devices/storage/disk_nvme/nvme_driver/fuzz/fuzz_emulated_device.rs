// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A shim layer for an EmulatedDevice to inject aribtrary responses for fuzzing the nvme driver.
use crate::get_raw_data;

use chipset_device::mmio::MmioIntercept;
use chipset_device::pci::PciConfigSpace;
use inspect::Inspect;
use inspect::InspectMut;
use pci_core::msi::MsiInterruptSet;
use user_driver::DeviceBacking;
use user_driver::emulated::{EmulatedDevice, Mapping, EmulatedDmaAllocator, DeviceSharedMemory};
use user_driver::interrupt::DeviceInterrupt;

/// An emulated device fuzzer
pub struct FuzzEmulatedDevice<T> {
    device: EmulatedDevice<T>,
}

impl<T: InspectMut> Inspect for FuzzEmulatedDevice<T> {
    fn inspect(&self, req: inspect::Request<'_>) {
        self.device.inspect(req);
    }
}

impl<T: PciConfigSpace + MmioIntercept> FuzzEmulatedDevice<T> {
    /// Creates a new emulated device, wrapping `device`, using the provided MSI controller.
    pub fn new(device: T, msi_set: MsiInterruptSet, shared_mem: DeviceSharedMemory) -> Self {
        Self {
            device: EmulatedDevice::new(device, msi_set, shared_mem),
        }
    }
}

impl<T: 'static + Send + InspectMut + MmioIntercept> DeviceBacking for FuzzEmulatedDevice<T> {
    type Registers = Mapping<T>;
    type DmaAllocator = EmulatedDmaAllocator;
    fn id(&self) -> &str {
        self.device.id()
    }
    fn map_bar(&mut self, n: u8) -> anyhow::Result<Self::Registers> {
        self.device.map_bar(n)
    }
    /// Returns an object that can allocate host memory to be shared with the device.
    fn host_allocator(&self) -> Self::DmaAllocator {
        self.device.host_allocator()
    }
    fn max_interrupt_count(&self) -> u32 {
        println!("Max interrupt count invoked");
        match get_arbitrary_interrupt_count() {
            Ok(count) => {
                println!("returning interrupt count of {}", count);
                return count;
            },
            Err(_e) => return self.device.max_interrupt_count(),
        }
    }
    fn map_interrupt(&mut self, msix: u32, _cpu: u32) -> anyhow::Result<DeviceInterrupt> {
        self.device.map_interrupt(msix, _cpu)
    }
}
