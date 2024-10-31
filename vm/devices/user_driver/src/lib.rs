// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Infrastructure for implementing PCI drivers in user mode.

// UNSAFETY: Manual memory management around buffers and mmap.
#![allow(unsafe_code)]

use inspect::Inspect;
use interrupt::DeviceInterrupt;
use memory::MemoryBlock;

pub mod backoff;
pub mod emulated;
pub mod interrupt;
pub mod lockmem;
pub mod memory;
pub mod vfio;

pub type DmaAllocator<T> = <T as DeviceBacking>::DmaAllocator;

/// An interface to access device hardware.
pub trait DeviceBacking: 'static + Send + Inspect {
    /// An object for accessing device registers.
    type Registers: 'static + DeviceRegisterIo + Inspect;
    /// An object for allocating host memory to share with the device.
    type DmaAllocator: 'static + HostDmaAllocator;

    /// Returns a device ID for diagnostics.
    fn id(&self) -> &str;

    /// Maps a BAR.
    fn map_bar(&mut self, n: u8) -> anyhow::Result<Self::Registers>;

    /// Returns an object that can allocate host memory to be shared with the device.
    fn host_allocator(&self) -> Self::DmaAllocator;

    /// Returns the maximum number of interrupts that can be mapped.
    fn max_interrupt_count(&self) -> u32;

    /// Maps a MSI-X interrupt for use, returning an object that can be used to
    /// wait for the interrupt to be signaled by the device.
    ///
    /// `cpu` is the CPU that the device should target with this interrupt.
    ///
    /// This can be called multiple times for the same interrupt without disconnecting
    /// previous mappings. The last `cpu` value will be used as the target CPU.
    fn map_interrupt(&mut self, msix: u32, cpu: u32) -> anyhow::Result<DeviceInterrupt>;
}

/// Access to device registers.
pub trait DeviceRegisterIo: Send + Sync {
    /// Reads a `u32` register.
    fn read_u32(&self, offset: usize) -> u32;
    /// Reads a `u64` register.
    fn read_u64(&self, offset: usize) -> u64;
    /// Writes a `u32` register.
    fn write_u32(&self, offset: usize, data: u32);
    /// Writes a `u64` register.
    fn write_u64(&self, offset: usize, data: u64);
    /// Returns base virtual address.
    fn base_va(&self) -> u64;
}

pub trait HostDmaAllocator: Send + Sync {
    /// Allocate a new block using default allocation strategy.
    fn allocate_dma_buffer(&self, len: usize) -> anyhow::Result<MemoryBlock>;

    /// Restore DMA bufer at pre-existing location.
    fn restore_dma_buffer(
        &mut self,
        len: usize,
        pfns: &[u64],
    ) -> anyhow::Result<MemoryBlock>;
}

pub mod save_restore {
    use mesh::payload::Protobuf;

    /// Saved state for the VFIO device user mode driver.
    #[derive(Protobuf, Clone, Debug)]
    #[mesh(package = "underhill")]
    pub struct VfioDeviceSavedState {
        #[mesh(1)]
        pub pci_id: String,
        #[mesh(2)]
        pub msix_info_count: u32,
    }
}
