// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A shim layer to fuzz responses from an emulated device.
use crate::arbitrary_data;

use chipset_device::mmio::MmioIntercept;
use chipset_device::pci::PciConfigSpace;
use inspect::Inspect;
use inspect::InspectMut;
use pci_core::msi::MsiInterruptSet;
use user_driver::DeviceBacking;
use user_driver::emulated::DeviceSharedMemory;
use user_driver::emulated::EmulatedDevice;
use user_driver::emulated::EmulatedDmaAllocator;
use user_driver::emulated::Mapping;
use user_driver::interrupt::DeviceInterrupt;

/// An EmulatedDevice fuzzer that requires a working EmulatedDevice backend.
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

/// Implementation for DeviceBacking trait.
/// Static is required here since the trait enforces static lifetime.
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

    /// Passthrough to backend or return arbitrary u32.
    fn max_interrupt_count(&self) -> u32 {
        // Case: Fuzz response
        if let Ok(true) = arbitrary_data::<bool>() {
            // Return an abritrary u32
            if let Ok(num) = arbitrary_data::<u32>() {
                return num;
            }
        }

        // Case: Passthrough
        return self.device.max_interrupt_count();
            
    }

    fn map_interrupt(&mut self, msix: u32, _cpu: u32) -> anyhow::Result<DeviceInterrupt> {
        self.device.map_interrupt(msix, _cpu)
    }
}
