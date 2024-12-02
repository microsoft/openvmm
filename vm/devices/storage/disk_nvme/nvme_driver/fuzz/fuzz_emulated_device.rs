// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A shim layer for an EmulatedDevice to inject aribtrary responses for fuzzing the nvme driver.

use anyhow::Result;
use arbitrary::Arbitrary;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pci::PciConfigSpace;
use inspect::Inspect;
use inspect::InspectMut;
use pci_core::msi::MsiInterruptSet;
use user_driver::DeviceBacking;
use user_driver::emulated::{EmulatedDevice, Mapping, EmulatedDmaAllocator, DeviceSharedMemory};
use user_driver::interrupt::DeviceInterrupt;

// TODO: Add a polling mechnanism here. Basically every time we hit the FuzzEmulatedDeviceAction
// we add the given action to the mapping of actions that exists. If there exists a map, we execute
// said action. If there is nothing we can go business as usual!
pub struct FuzzEmulatedDeviceActionsMap {
    actions: FuzzEmulatedDeviceAction,
}

impl FuzzEmulatedDeviceActionsMap {
    /// Default with no Action
    pub fn new() -> Self {
        Self {
            actions: None,
        }
    }

    // Returns true if an action exists
    pub fn has_action(&self) -> bool {
        self.action == None
    }

    // Consumes the action if anything exists. Returns an action if one exists, None otherwise
    pub fn consume_action(&mut self) -> FuzzEmulatedDeviceAction {
        self.take()   
    }

    // Sets action to the provided action
    pub fn set_action(&mut self, action: FuzzEmulatedDeviceAction) -> bool {
        self.actions = action;
    }
}

/// An emulated device fuzzer
pub struct FuzzEmulatedDevice<T> {
    device: EmulatedDevice<T>,
    pending_action: FuzzEmulatedDeviceActionsMap,
}

impl<T: InspectMut> Inspect for FuzzEmulatedDevice<T> {
    fn inspect(&self, req: inspect::Request<'_>) {
        self.device.inspect(req);
    }
}

impl<T: PciConfigSpace + MmioIntercept> FuzzEmulatedDevice<T> {
    /// Creates a new emulated device, wrapping `device`, using the provided MSI controller.
    pub fn new(mut device: T, msi_set: MsiInterruptSet, shared_mem: DeviceSharedMemory) -> Self {
        Self {
            device: EmulatedDevice::new(device, msi_set, shared_mem),
            pending_action: FuzzEmulatedDeviceActionsMap::new(),
        }
    }

    /// Sets up the mock to execute the given action on the next call to the appropriate function
    pub fn execute_action(&self, action: FuzzEmulatedDeviceAction) {
        self.pending_action.set_action(action);
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

    /// Returns the max_interrupt_count using the set mock response. Passthrough if nothing was
    /// set.
    fn max_interrupt_count(&self) -> u32 {
        // Check if we wanted to mock the given action. If so, intercept the call
        if self.pending_action.has_action() {
            let action = self.pending_action.consume_action();

            if let FuzzEmulatedDeviceAction::MaxInterruptCount(count) = action {
                return count;        
            }
        }

        // Passthrough branch
        self.device.max_interrupt_count()
    }

    fn map_interrupt(&mut self, msix: u32, _cpu: u32) -> anyhow::Result<DeviceInterrupt> {
        self.device.map_interrupt(msix, _cpu)
    }
}

#[derive(Debug, Arbitrary)]
pub enum FuzzEmulatedDeviceAction {
    MaxInterruptCount {
        max_interrupt_count: u32,
    }
}
