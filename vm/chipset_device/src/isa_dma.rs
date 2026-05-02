// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ISA DMA controller capability exposed by chipset devices.

/// ISA DMA transfer direction.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum IsaDmaTransferDirection {
    /// Device is writing data into guest memory.
    Write,
    /// Device is reading data from guest memory.
    Read,
}

/// Location of a DMA transfer buffer in guest memory.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct IsaDmaTransferBuffer {
    /// Guest physical address of the DMA buffer.
    pub address: u64,
    /// Transfer size in bytes.
    pub size: usize,
}

/// Optional capability implemented by chipset devices that expose an ISA DMA
/// controller programming interface.
pub trait IsaDmaController {
    /// Check the value of the DMA channel's configured transfer size.
    fn check_transfer_size(&mut self, channel_number: usize) -> u16;

    /// Request access to an ISA DMA channel buffer.
    ///
    /// Returns `None` when the channel is not configured for this transfer.
    fn request(
        &mut self,
        channel_number: usize,
        direction: IsaDmaTransferDirection,
    ) -> Option<IsaDmaTransferBuffer>;

    /// Signal that DMA transfer on the given channel has completed.
    fn complete(&mut self, channel_number: usize);
}
