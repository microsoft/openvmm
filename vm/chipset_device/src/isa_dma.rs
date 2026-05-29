// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ISA DMA controller capability exposed by chipset devices.

pub use vmcore::isa_dma_channel::IsaDmaBuffer as IsaDmaTransferBuffer;
pub use vmcore::isa_dma_channel::IsaDmaDirection as IsaDmaTransferDirection;

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
