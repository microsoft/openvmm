// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A shim layer to fuzz responses from an emulated device.
use crate::arbitrary_bool;
use crate::get_raw_data;

use arbitrary::Unstructured;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pci::PciConfigSpace;
use inspect::Inspect;
use inspect::InspectMut;
use pci_core::msi::MsiInterruptSet;
use user_driver::DeviceBacking;
use user_driver::emulated::{EmulatedDevice, Mapping, EmulatedDmaAllocator, DeviceSharedMemory};
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
        if arbitrary_bool() {
            match get_raw_data(size_of::<u32>()) {
                Ok(data) => {
                    // Lazy create unstructured data
                    let mut u = Unstructured::new(&data);

                    // Generate an arbitrary return value
                    match u.arbitrary() {
                        Ok(arb) => { return arb; }
                        Err(_e) => {}  // Ignore errors
                    }
                }
                Err(_e) => {}  // Ignore errors
            }
        }

        // Case: Passthrough
        return self.device.max_interrupt_count();
            
    }

    fn map_interrupt(&mut self, msix: u32, _cpu: u32) -> anyhow::Result<DeviceInterrupt> {
        self.device.map_interrupt(msix, _cpu)
    }
}
