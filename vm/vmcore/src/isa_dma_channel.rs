// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Infrastructure to support legacy ISA DMA channels.

/// ISA DMA transfer direction
#[derive(PartialEq, Debug)]
pub enum IsaDmaDirection {
    /// Device is writing data to the buffer
    Write,
    /// Device is reading data from the buffer
    Read,
}

/// Location of DMA buffer in guest memory
#[derive(Debug)]
pub struct IsaDmaBuffer {
    /// GPA of the DMA buffer
    pub address: u64,
    /// Size of the DMA buffer
    pub size: usize,
}

/// A handle to an ISA DMA channel.
///
/// This trait does not "leak" which partiuclar ISA DMA channel a device is
/// connected to.
///
/// Devices that use ISA DMA should simply accept an instance of `Box<dyn
/// IsaDmaChannel>`, leaving the details of DMA channel assignment to
/// upper-level system init code that backs the `IsaDmaChannel` trait object.
pub trait IsaDmaChannel: Send {
    /// Check the value of the DMA channel's configured transfer size.
    fn check_transfer_size(&mut self) -> u16;
    /// Requests an access to ISA DMA channel buffer.
    ///
    /// Returns `None` if the channel has not been configured correctly.
    fn request(&mut self, direction: IsaDmaDirection) -> Option<IsaDmaBuffer>;
    /// Signals to the DMA controller that the transfer is concluded.
    fn complete(&mut self);
}

/// A floating DMA channel that is not connected to any device.
pub struct FloatingDmaChannel;

impl IsaDmaChannel for FloatingDmaChannel {
    fn check_transfer_size(&mut self) -> u16 {
        0
    }

    fn request(&mut self, direction: IsaDmaDirection) -> Option<IsaDmaBuffer> {
        tracing::warn!(?direction, "called `request` on floating DMA channel");
        None
    }

    fn complete(&mut self) {
        tracing::warn!("called `complete` on floating DMA channel");
    }
}
