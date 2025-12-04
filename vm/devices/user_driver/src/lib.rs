// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Infrastructure for implementing PCI drivers in user mode.

// UNSAFETY: Manual memory management around buffers and mmap.
#![expect(unsafe_code)]
#![expect(missing_docs)]

use inspect::Inspect;
use interrupt::DeviceInterrupt;
use memory::MemoryBlock;
use std::sync::Arc;

pub mod backoff;
pub mod interrupt;
pub mod lockmem;
pub mod memory;
pub mod page_allocator;
pub mod vfio;
pub mod dma;

pub enum DmaPool {
    Ephemeral,
    Persistent,
}

/// An interface to access device hardware.
pub trait DeviceBacking: 'static + Send + Inspect {
    /// An object for accessing device registers.
    type Registers: 'static + DeviceRegisterIo + Inspect;

    /// Returns a device ID for diagnostics.
    fn id(&self) -> &str;

    /// Maps a BAR.
    fn map_bar(&mut self, n: u8) -> anyhow::Result<Self::Registers>;

    /// DMA Client for the device.
    fn dma_client(&self) -> Arc<dyn DmaClient>;

    /// Overloaded DMA Client for the device, based on the requested pool.
    ///
    /// Default implementation returns an error as this is only currently implemented
    /// by VfioDevice's implementation.
    fn dma_client_for(&self, _pool: DmaPool) -> anyhow::Result<Arc<dyn DmaClient>> {
        anyhow::bail!("multiple dma clients are not supported by this DmaClient");
    }

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

    /// Unmaps and disables all previously mapped interrupts.
    ///
    /// Default implementation is a no-op for backends that do not support interrupt unmapping.
    fn unmap_all_interrupts(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Access to device registers.
pub trait DeviceRegisterIo: Send + Sync {
    /// Returns the length of the register space.
    fn len(&self) -> usize;
    /// Reads a `u32` register.
    fn read_u32(&self, offset: usize) -> u32;
    /// Reads a `u64` register.
    fn read_u64(&self, offset: usize) -> u64;
    /// Writes a `u32` register.
    fn write_u32(&self, offset: usize, data: u32);
    /// Writes a `u64` register.
    fn write_u64(&self, offset: usize, data: u64);
}

/// Device interfaces for DMA.
pub trait DmaClient: Send + Sync + Inspect {
    /// Allocate a new DMA buffer. This buffer must be zero initialized.
    ///
    /// TODO: string tag for allocation?
    /// TODO: contiguous vs non-contiguous? (both on the request side, and if
    ///       a request contiguous allocation cannot be fulfilled)
    fn allocate_dma_buffer(&self, total_size: usize) -> anyhow::Result<MemoryBlock>;

    /// Attach all previously allocated memory blocks.
    fn attach_pending_buffers(&self) -> anyhow::Result<Vec<MemoryBlock>>;
}
